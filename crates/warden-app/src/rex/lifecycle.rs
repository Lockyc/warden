//! The agent tab list's lifecycle: new, wake, sleep, close, reopen, fork and rename an agent
//! tab, and follow windows that change in Rex — each one pass over the pane's records under the
//! pane's hold ([`RexHub::agent_op`]) — plus what the list shows ([`agent_list`]).
//!
//! The commands run here: a worker does the Rex work, the answer lands on the main thread in
//! every window holding the tab ([`Registry::sync_agents`](crate::registry::Registry::sync_agents)),
//! and `warden:agents` carries the list after every result, every reported resume command, and
//! every window or status change Rex reports for a followed pane.

use std::sync::Mutex;
use std::time::Instant;

use rex_client::{Endpoint, RexError};
use tauri::{AppHandle, Emitter, Manager};

use super::agents::{self, AgentRecord, AgentStore, RecordState};
use super::attention::Attention;
use super::spawn::unix_now;
use super::windows::{AgentState, BlockStatus, BlockedKind};
use super::{budget_error, lock, HubEvent, RexHub, OPEN_BUDGET};
use crate::registry::{AgentsOpened, PaneIdx, RexOpen};
use crate::ManagerState;

/// Why an agent tab can't sleep, or closes for good: it never reported its resume command.
pub const NO_RESUME: &str = "this agent hasn't reported how to resume it";

const NO_AGENT: &str = "that agent tab is gone";

/// One lifecycle step on a pane's agent tabs, by record id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOp {
    /// A new window typing the tab's `cmd`, selected, last in the ungrouped tabs.
    New,
    /// Bring an asleep agent back (selecting a live one).
    Wake(String),
    /// Close the agent's window and keep it asleep, to wake later.
    Sleep(String),
    /// As sleep, into the closed fold; without a resume command it goes for good, `confirmed`.
    Close { id: String, confirmed: bool },
    /// Wake a closed agent, the most recently closed when `None`.
    Reopen(Option<String>),
    /// A new tab below the agent, in its group, selected, resuming the same conversation as an
    /// independent branch ([`agents::fork_command`]); the agent itself is left as it is.
    Fork(String),
    /// Name the tab; a live one's window is relabelled in Rex.
    Rename(String, String),
    /// Follow the session's windows as they are now.
    Refresh,
    /// The agent's program exited and its window was let go ([`RexHub::end_agent_if_exited`]):
    /// as a confirmed close, without the Rex call. Closed even when its session ended with it,
    /// which a refresh would read as a lost session and sleep it, waking it on the next open.
    Exited(String),
}

/// Why `op` can't run on `records`: sleeping, closing unconfirmed or forking an agent that never
/// reported its resume command ([`NO_RESUME`]); forking one whose harness can't fork
/// ([`agents::CANT_FORK`]) or a tab the list doesn't show ([`NO_AGENT`]).
pub fn refusal(op: &AgentOp, records: &[AgentRecord]) -> Option<&'static str> {
    if let AgentOp::Fork(id) = op {
        let Some(r) = records
            .iter()
            .find(|r| r.id == *id && r.state != RecordState::Closed)
        else {
            return Some(NO_AGENT);
        };
        return match r.resume.as_deref() {
            None => Some(NO_RESUME),
            Some(resume) => agents::fork_command(resume).err(),
        };
    }
    let id = match op {
        AgentOp::Sleep(id)
        | AgentOp::Close {
            id,
            confirmed: false,
        } => id,
        _ => return None,
    };
    records
        .iter()
        .any(|r| r.id == *id && r.state == RecordState::Live && r.resume.is_none())
        .then_some(NO_RESUME)
}

/// How an op failed: refused before any Rex work, or Rex said no.
enum Failed {
    Refused(&'static str),
    Rex(RexError),
}

/// A close's window, asked of Rex once the list without it is shown and the pane released, and
/// its record as it was, to put back if Rex refuses.
struct Closing {
    session: Option<(Endpoint, String)>,
    sid: String,
    window: String,
    before: AgentRecord,
}

impl From<RexError> for Failed {
    fn from(e: RexError) -> Self {
        Failed::Rex(e)
    }
}

impl RexHub {
    /// Run `op` on `req`'s agent pane: reconcile its records with its session, act, then settle
    /// which one is selected — the one the op shows, else the selected one if live, else its
    /// nearest live neighbour in list order — and bind the live ones as an open does. The whole
    /// op holds the pane under one [`OPEN_BUDGET`]; the store is never held across a call. `Err`
    /// is the refusal or failure, as the tab shows it.
    pub fn agent_op(
        &self,
        store: &Mutex<AgentStore>,
        req: &RexOpen,
        op: &AgentOp,
        now: u64,
    ) -> Result<AgentsOpened, String> {
        let mut last = AgentsOpened::default();
        self.agent_op_shown(store, req, op, now, |opened| last = opened)?;
        Ok(last)
    }

    /// [`RexHub::agent_op`], handing `shown` each list it stores as it stores it. A close stores
    /// and shows its list before asking Rex to close the window — Rex answers only once the
    /// program in it has exited, which an agent shutting down cleanly makes take a second — and,
    /// if Rex then refuses, shows the record put back as it was before returning the refusal.
    /// That wait is outside the pane's hold, so the next op (⌘T right after ⌘W) doesn't queue
    /// behind the exit; the window is closing ([`RexHub::windows`] leaves it out) meanwhile.
    pub fn agent_op_shown(
        &self,
        store: &Mutex<AgentStore>,
        req: &RexOpen,
        op: &AgentOp,
        now: u64,
        mut shown: impl FnMut(AgentsOpened),
    ) -> Result<(), String> {
        let deadline = Instant::now() + OPEN_BUDGET;
        let done = match self.inner.hold_pane(&req.pane_key, Some(deadline)) {
            Ok(_held) => self.agent_op_until(store, req, op, now, deadline, &mut shown),
            Err(e) => Err(Failed::Rex(e)),
        }
        .and_then(|closing| match closing {
            Some(c) => self.finish_close(store, req, c, &mut shown),
            None => Ok(()),
        });
        match done {
            Ok(()) => Ok(()),
            Err(Failed::Refused(why)) => Err(why.to_owned()),
            Err(Failed::Rex(e)) => Err(super::spawn::open_error(
                &req.server,
                &budget_error(deadline, e),
            )),
        }
    }

    /// The main panes followed as agent tabs (opened as agent panes) whose session is
    /// `session_id` on `server`.
    pub fn agent_panes(&self, server: &str, session_id: &str) -> Vec<String> {
        let st = self.inner.lock();
        let mut panes: Vec<String> = st
            .agent_panes
            .iter()
            .filter(|p| {
                st.record(p)
                    .is_some_and(|r| r.server == server && r.session_id == session_id)
            })
            .cloned()
            .collect();
        panes.sort();
        panes
    }

    fn agent_op_until(
        &self,
        store: &Mutex<AgentStore>,
        req: &RexOpen,
        op: &AgentOp,
        now: u64,
        deadline: Instant,
        shown: &mut dyn FnMut(AgentsOpened),
    ) -> Result<Option<Closing>, Failed> {
        let (project, server) = (req.pane_key.as_str(), req.server.as_str());
        let mut held = lock(store).records(project);
        if let Some(why) = refusal(op, &held) {
            return Err(Failed::Refused(why));
        }
        let mut session = self.current_session(project, server, deadline)?;
        // A restored session is retired here as an open retires it, so an op never adds an agent
        // to bare shells a later open would close along with it.
        if let Some((_, sid)) = session.as_ref().filter(|_| self.is_restored(project)) {
            if self.retire_restored(store, project, server, &sid.clone(), now, deadline)? {
                session = None;
            }
            held = lock(store).records(project);
        }
        let windows = match &session {
            Some((_, sid)) => self.windows(server, sid),
            None => Vec::new(),
        };
        let mut records = agents::reconcile(project, &held, &windows, session.is_some(), now);
        let at = |records: &[AgentRecord], id: &str| {
            records
                .iter()
                .position(|r| r.id == id)
                .ok_or(Failed::Refused(NO_AGENT))
        };
        let mut want = None;
        // A tab closing for good is dropped once the selection has leaned off it.
        let mut drop = None;
        // A close's window, asked of Rex once the list without it is shown, and its record as it
        // was, to put back if Rex refuses.
        let mut closing = None;
        match op {
            AgentOp::Refresh => {}
            AgentOp::Exited(id) => {
                // Gone already when its window closed in a live session without a resume command.
                // Only an agent stored live exited: one already asleep had its window closed by
                // warden (a restored session's open), and stays asleep for its row to wake.
                let was_live = held
                    .iter()
                    .any(|r| r.id == *id && r.state == RecordState::Live);
                let open = records
                    .iter_mut()
                    .filter(|_| was_live)
                    .find(|r| r.id == *id && r.state != RecordState::Closed);
                if let Some(r) = open {
                    r.window_id = None;
                    r.block_id = None;
                    r.state = RecordState::Closed;
                    r.closed_at = Some(now);
                    if r.resume.is_none() {
                        drop = Some(r.id.clone());
                    }
                }
            }
            AgentOp::New => {
                let made = match &session {
                    Some((_, sid)) => self.inner.new_window_until(
                        server,
                        sid,
                        &req.dir,
                        req.startup.as_deref(),
                        None,
                        deadline,
                    )?,
                    None => {
                        let t = self.inner.ensure_until(
                            project,
                            server,
                            &req.dir,
                            req.startup.as_deref(),
                            &req.label,
                            None,
                            deadline,
                        )?;
                        let w = self.window_of(server, &t.session_id, &t.block_id)?;
                        session = Some((t.endpoint, t.session_id));
                        w
                    }
                };
                let sid = session
                    .as_ref()
                    .map(|(_, s)| s.as_str())
                    .unwrap_or_default();
                let mut windows = self.windows(server, sid);
                if !windows.iter().any(|w| w.window_id == made.window_id) {
                    windows.push(made.clone());
                }
                records = agents::reconcile(project, &records, &windows, true, now);
                want = records
                    .iter()
                    .position(|r| r.window_id.as_deref() == Some(made.window_id.as_str()));
            }
            AgentOp::Wake(id) | AgentOp::Reopen(Some(id)) => {
                let i = at(&records, id)?;
                if records[i].state != RecordState::Live {
                    self.wake_until(req, &mut session, &mut records[i], deadline)?;
                }
                want = Some(i);
            }
            AgentOp::Reopen(None) => {
                let i = records
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.state == RecordState::Closed)
                    .max_by_key(|(_, r)| r.closed_at)
                    .map(|(i, _)| i)
                    .ok_or(Failed::Refused(NO_AGENT))?;
                self.wake_until(req, &mut session, &mut records[i], deadline)?;
                want = Some(i);
            }
            AgentOp::Sleep(id) => {
                let i = at(&records, id)?;
                if records[i].state == RecordState::Live {
                    self.close_window_of(server, session.as_ref(), &mut records[i], deadline)?;
                    records[i].state = RecordState::Asleep;
                }
            }
            AgentOp::Close { id, .. } => {
                let i = at(&records, id)?;
                let r = &mut records[i];
                if let (Some((_, sid)), Some(w)) = (&session, &r.window_id) {
                    closing = Some((sid.clone(), w.clone(), r.clone()));
                }
                r.window_id = None;
                r.block_id = None;
                if r.resume.is_none() {
                    drop = Some(r.id.clone());
                }
                if r.state != RecordState::Closed {
                    r.state = RecordState::Closed;
                    r.closed_at = Some(now);
                }
            }
            AgentOp::Fork(id) => {
                // Refused once reconcile closed it too: its window went in Rex meanwhile.
                let si = at(&records, id)?;
                let src = records[si].clone();
                if src.state == RecordState::Closed {
                    return Err(Failed::Refused(NO_AGENT));
                }
                let cmd = src
                    .resume
                    .as_deref()
                    .ok_or(Failed::Refused(NO_RESUME))
                    .and_then(|r| agents::fork_command(r).map_err(Failed::Refused))?;
                let cmd = warden_config::apply_resume(req.resume.as_deref(), &cmd);
                let name = match src.name.as_str() {
                    "" => "fork".to_owned(),
                    n => format!("{n} fork"),
                };
                let w = self.open_window_until(req, &mut session, &cmd, Some(&name), deadline)?;
                // Directly below the source: everything after it moves down one.
                for r in records.iter_mut().filter(|r| r.position > src.position) {
                    r.position += 1;
                }
                records.push(AgentRecord {
                    id: agents::new_id(),
                    project: project.to_owned(),
                    name,
                    group: src.group.clone(),
                    position: src.position + 1,
                    resume: None,
                    state: RecordState::Live,
                    window_id: Some(w.window_id),
                    block_id: w.block_id,
                    closed_at: None,
                    selected: false,
                    // Its source's first fork, directly below it.
                    lineage: std::iter::once(src.id.clone())
                        .chain(src.lineage.iter().cloned())
                        .collect(),
                    folded: false,
                });
                // A fork is selected, so its source unfolds to show it.
                records[si].folded = false;
                want = Some(records.len() - 1);
            }
            AgentOp::Rename(id, name) => {
                let i = at(&records, id)?;
                let name = name.trim();
                let window = records[i]
                    .window_id
                    .clone()
                    .filter(|_| records[i].state == RecordState::Live);
                if let (Some((_, sid)), Some(w)) = (&session, window) {
                    self.set_window_label_until(server, sid, &w, name, deadline)?;
                }
                if !name.is_empty() {
                    records[i].name = name.to_owned();
                }
            }
        }
        if let Some((_, window, _)) = &closing {
            self.mark_closing(server, window);
        }
        shown(self.store_settled(store, project, session.as_ref(), &held, records, want, drop));
        Ok(closing.map(|(sid, window, before)| Closing {
            session,
            sid,
            window,
            before,
        }))
    }

    /// Ask Rex to close `c`'s window, waiting for its program to exit, outside the pane's hold.
    /// A refusal takes the hold again and puts the record back as it was, shown. A timeout is
    /// no refusal: the close was sent, and Rex answers only once the program has exited, which a
    /// slow one makes take longer than the call's wait.
    fn finish_close(
        &self,
        store: &Mutex<AgentStore>,
        req: &RexOpen,
        c: Closing,
        shown: &mut dyn FnMut(AgentsOpened),
    ) -> Result<(), Failed> {
        let (project, server) = (req.pane_key.as_str(), req.server.as_str());
        let e = match self.close_window(server, &c.sid, &c.window) {
            Ok(()) | Err(RexError::Timeout) => return Ok(()),
            Err(e) => e,
        };
        let _held = self
            .inner
            .hold_pane(project, Some(Instant::now() + OPEN_BUDGET))?;
        let held = lock(store).records(project);
        let mut records: Vec<AgentRecord> = held
            .iter()
            .filter(|r| r.id != c.before.id)
            .cloned()
            .collect();
        records.push(c.before);
        let want = Some(records.len() - 1);
        let session = c.session.as_ref();
        shown(self.store_settled(store, project, session, &held, records, want, None));
        Err(e.into())
    }

    /// Settle `records` (`project`'s, changed from `held`) as [`settle`] does around `want`, drop
    /// record `drop` once the selection has leaned off it, bind the live ones and store them.
    #[allow(clippy::too_many_arguments)]
    fn store_settled(
        &self,
        store: &Mutex<AgentStore>,
        project: &str,
        session: Option<&(Endpoint, String)>,
        held: &[AgentRecord],
        mut records: Vec<AgentRecord>,
        want: Option<usize>,
        drop: Option<String>,
    ) -> AgentsOpened {
        renumber(&mut records);
        let shown = settle(&mut records, want, false);
        if let Some(id) = drop {
            records.retain(|r| r.id != id);
        }
        let shown = shown.and_then(|i| {
            let id = records.get(i).map(|r| r.id.clone())?;
            records.iter().position(|r| r.id == id)
        });
        let opened = self.bind_agents(project, session, &records, shown);
        lock(store).merge(held, records);
        opened
    }

    /// `project`'s session on `server`: the one the hub already follows, read without a call,
    /// else found ([`RexHub::find_session_until`]). `None` once it is gone.
    fn current_session(
        &self,
        project: &str,
        server: &str,
        deadline: Instant,
    ) -> Result<Option<(Endpoint, String)>, RexError> {
        let mapped = self
            .inner
            .lock()
            .store
            .get(project)
            .filter(|r| r.server == server)
            .map(|r| r.session_id.clone());
        if let Some(sid) = mapped {
            let followed = self.inner.watching(server, &sid) && self.session_alive(project);
            if let Some((_, endpoint)) = self.inner.current(server).filter(|_| followed) {
                return Ok(Some((endpoint, sid)));
            }
        }
        self.find_session_until(project, server, deadline)
    }

    /// Close `record`'s window, if it has one in `session`, and unhook the record from it.
    fn close_window_of(
        &self,
        server: &str,
        session: Option<&(Endpoint, String)>,
        record: &mut AgentRecord,
        deadline: Instant,
    ) -> Result<(), RexError> {
        if let (Some((_, sid)), Some(w)) = (session, record.window_id.as_deref()) {
            self.close_window_until(server, sid, w, deadline)?;
        }
        record.window_id = None;
        record.block_id = None;
        Ok(())
    }
}

/// Why a server can't sleep: warden has no connection to it.
pub const NOT_RUNNING: &str = "not running";

/// Why a server can't sleep: Rex stops only for a client on its own socket.
pub const NOT_LOCAL: &str = "stops only from its own Mac";

/// What sleeping a server loses for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LossKind {
    /// A live agent tab that never reported its resume command.
    Agent,
    /// A session warden doesn't own: Rex.app's, the phone's, an unclaimed row's.
    Session,
}

/// One thing a server's sleep loses, by the name the confirm lists it under.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LossItem {
    pub kind: LossKind,
    pub name: String,
}

/// Sleeping a server or a project: the agent records that go asleep, and what is lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SleepPlan {
    pub sleep: Vec<String>,
    pub lose: Vec<LossItem>,
}

/// How [`RexHub::sleep_server`] or [`RexHub::sleep_project`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slept {
    /// The server stopped, or the project's sessions ended.
    Done,
    /// Nothing ended: there's now a loss the plan didn't list — this plan, to confirm.
    Unconfirmed(SleepPlan),
}

impl SleepPlan {
    /// The losses not among `accepted` (names the user confirmed). Empty: go ahead.
    pub fn unconfirmed(&self, accepted: &[String]) -> Vec<&LossItem> {
        self.lose
            .iter()
            .filter(|l| !accepted.contains(&l.name))
            .collect()
    }
}

impl RexHub {
    /// Whether `server` can sleep now: warden is connected to it ([`NOT_RUNNING`]) over its
    /// local socket, the one transport Rex takes `server.stop` on ([`NOT_LOCAL`]).
    pub fn stoppable(&self, server: &str) -> Result<(), &'static str> {
        match self.inner.current(server) {
            None => Err(NOT_RUNNING),
            Some((_, Endpoint::Unix(_))) => Ok(()),
            Some(_) => Err(NOT_LOCAL),
        }
    }

    /// What sleeping `server` does, from its session list as last heard: per listed session, the
    /// main pane mapped to it sleeps its live resumable agent records and loses the rest, matched
    /// against the session's windows — as the hub follows them, else read now
    /// ([`RexHub::read_windows`]), so a window no record knows is listed too; a split mapped to it
    /// is scratch, and ends unlisted; one nothing maps is a session lost. `title` names a tab by its key. Reads
    /// the network (worker only); `Err` when a session's windows can't be read, since the plan
    /// would then miss what it loses.
    pub fn plan_server_sleep(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Result<SleepPlan, RexError> {
        self.plan_on(store, server, None, title)
    }

    /// What sleeping project `panes` (one tab's pane keys) on `server` does: as
    /// [`RexHub::plan_server_sleep`], over only the sessions those panes map to, so no other
    /// project's session is a loss. Empty once its sessions are gone.
    pub fn plan_project_sleep(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        panes: &[String],
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Result<SleepPlan, RexError> {
        self.plan_on(store, server, Some(panes), title)
    }

    /// The one planner behind both sleeps: `server`'s listed sessions, narrowed to those `panes`
    /// map to when given.
    fn plan_on(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        panes: Option<&[String]>,
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Result<SleepPlan, RexError> {
        let (mut sessions, mut mapped) = {
            let st = self.inner.lock();
            let sessions = st.sessions(server).map(<[_]>::to_vec).unwrap_or_default();
            (sessions, st.store.panes_on(server))
        };
        if let Some(panes) = panes {
            mapped.retain(|(k, _)| panes.contains(k));
            sessions.retain(|s| mapped.iter().any(|(_, sid)| *sid == s.id));
        }
        let mut plan = SleepPlan::default();
        let (mut agents, mut unowned) = (Vec::new(), Vec::new());
        for s in &sessions {
            let label = || match s.label.as_str() {
                "" => s.id.clone(),
                l => l.to_owned(),
            };
            let panes: Vec<&str> = mapped
                .iter()
                .filter(|(_, sid)| *sid == s.id)
                .map(|(k, _)| k.as_str())
                .collect();
            if panes.is_empty() {
                unowned.push(LossItem {
                    kind: LossKind::Session,
                    name: label(),
                });
            }
            for key in panes {
                if crate::registry::tab_of_secondary(key).is_some() {
                    continue;
                }
                let windows = if self.inner.watching(server, &s.id) {
                    self.windows(server, &s.id)
                } else {
                    self.read_windows(server, &s.id)?
                };
                let records = agents::reconcile(key, &lock(store).records(key), &windows, true, 0);
                let tab = title(key).unwrap_or_else(|| key.to_owned());
                for r in agents::in_list_order(&records) {
                    match (r.state, &r.resume) {
                        (RecordState::Live, Some(_)) => plan.sleep.push(r.id),
                        (RecordState::Live, None) => agents.push(LossItem {
                            kind: LossKind::Agent,
                            name: format!("{tab} · {}", r.name),
                        }),
                        _ => {}
                    }
                }
            }
        }
        plan.lose = [agents, unowned].concat();
        Ok(plan)
    }

    /// Sleep `server` as `plan` says: each of its records, one at a time under its pane's hold,
    /// has its window closed and is stored asleep — so a sleep cut short anywhere leaves every
    /// record matching Rex, and a refused stop leaves the agents asleep rather than stranded —
    /// then `server.stop` — unless planning again just before it ([`RexHub::plan_server_sleep`],
    /// `title` naming tabs) finds a loss `plan` didn't list (a window opened meanwhile): then
    /// nothing stops, the slept records stay asleep, and the answer is that plan, to confirm.
    /// Once Rex has stopped, the hub forgets the server's sessions and doesn't reconnect: only a
    /// tab open starts it again.
    pub fn sleep_server(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        plan: &SleepPlan,
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Slept, RexError> {
        self.sleep_records(store, server, &plan.sleep)?;
        let now = self.plan_server_sleep(store, server, title)?;
        if let Some(now) = unconfirmed_since(plan, now) {
            return Ok(Slept::Unconfirmed(now));
        }
        let (client, _) = self.inner.current(server).ok_or(RexError::Unreachable)?;
        self.inner.lock().stopping.insert(server.to_owned());
        log::info!(
            "Rex {server}: sleeping server ({} agent(s))",
            plan.sleep.len()
        );
        match client.call("server.stop", serde_json::json!({})) {
            Ok(_) => {
                drop(client);
                self.inner.forget_server(server);
                Ok(Slept::Done)
            }
            Err(e) => {
                self.inner.lock().stopping.remove(server);
                Err(e)
            }
        }
    }

    /// Sleep project `panes` (one tab's pane keys) on `server` as `plan` says: its records sleep
    /// as [`RexHub::sleep_server`]'s do, then — unless planning again
    /// ([`RexHub::plan_project_sleep`]) finds a loss `plan` didn't list — each pane's session is
    /// destroyed. The records stay, asleep, for their rows to wake.
    pub fn sleep_project(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        panes: &[String],
        plan: &SleepPlan,
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Slept, RexError> {
        self.sleep_records(store, server, &plan.sleep)?;
        let now = self.plan_project_sleep(store, server, panes, title)?;
        if let Some(now) = unconfirmed_since(plan, now) {
            return Ok(Slept::Unconfirmed(now));
        }
        for key in panes {
            // A session no longer listed (a stopped server) has nothing to end; its stale mapping
            // is dropped without reaching for the server. A restored one is listed but reads as
            // gone; it holds only bare shells, and ends with the project.
            if self.session_alive(key) || self.is_restored(key) {
                self.destroy(key)?;
            } else {
                self.inner.forget(key);
            }
        }
        log::info!(
            "Rex {server}: project slept ({} agent(s), {} pane(s))",
            plan.sleep.len(),
            panes.len()
        );
        Ok(Slept::Done)
    }

    /// Put records `ids` on `server` to sleep, one at a time under its pane's hold: its window
    /// closed, stored asleep — so a sleep cut short anywhere leaves every record matching Rex.
    fn sleep_records(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        ids: &[String],
    ) -> Result<(), RexError> {
        for id in ids {
            let Some(project) = lock(store).project_of_record(id).filter(|p| {
                self.inner
                    .lock()
                    .store
                    .get(p)
                    .is_some_and(|r| r.server == server)
            }) else {
                continue;
            };
            let deadline = Instant::now() + OPEN_BUDGET;
            let _held = self.inner.hold_pane(&project, Some(deadline))?;
            let before = lock(store).records(&project);
            let mut after = before.clone();
            let Some(r) = after
                .iter_mut()
                .find(|r| r.id == *id && r.state == RecordState::Live)
            else {
                continue;
            };
            let session = self
                .inner
                .lock()
                .store
                .get(&project)
                .map(|m| m.session_id.clone());
            if let (Some(sid), Some(w)) = (session, r.window_id.as_deref()) {
                self.close_window_until(server, &sid, w, deadline)?;
            }
            r.window_id = None;
            r.block_id = None;
            r.state = RecordState::Asleep;
            lock(store).merge(&before, after);
        }
        Ok(())
    }
}

/// `now` when it loses something `planned` didn't list (a window opened since), else `None`.
fn unconfirmed_since(planned: &SleepPlan, now: SleepPlan) -> Option<SleepPlan> {
    let listed: Vec<String> = planned.lose.iter().map(|l| l.name.clone()).collect();
    (!now.unconfirmed(&listed).is_empty()).then_some(now)
}

/// Number `records`' positions in list order ([`agents::in_list_order`]).
fn renumber(records: &mut [AgentRecord]) {
    let order = agents::in_list_order(records);
    for r in records.iter_mut() {
        if let Some(p) = order.iter().position(|o| o.id == r.id) {
            r.position = p as u32;
        }
    }
}

/// Settle which of `records` (one project's) a pane shows and which is selected — at most one:
/// `want`, else the selected one while it is live, else its nearest live neighbour in list order,
/// else the first live one. The one shown becomes the selected one, except that with
/// `keep_asleep_choice` (an open) an asleep selected record stays selected while its neighbour
/// shows, since an open never wakes it. With nothing live the choice stays, unless it closed. The
/// one decider for both an open and every [`AgentOp`].
pub(super) fn settle(
    records: &mut [AgentRecord],
    want: Option<usize>,
    keep_asleep_choice: bool,
) -> Option<usize> {
    let live = |r: &AgentRecord| r.state == RecordState::Live;
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by_key(|&i| records[i].position);
    let chosen = records.iter().position(|r| r.selected);
    let shown = want.or_else(|| match chosen {
        Some(c) if live(&records[c]) => Some(c),
        Some(c) => {
            let at = order.iter().position(|&i| i == c)?;
            let flags: Vec<bool> = order.iter().map(|&i| live(&records[i])).collect();
            shell_core::pick_live_neighbour(at, &flags).map(|k| order[k])
        }
        None => order.iter().copied().find(|&i| live(&records[i])),
    });
    let keep = keep_asleep_choice
        && want.is_none()
        && chosen.is_some_and(|c| records[c].state == RecordState::Asleep);
    for (j, r) in records.iter_mut().enumerate() {
        match shown {
            Some(i) if !keep => r.selected = j == i,
            _ if r.state == RecordState::Closed => r.selected = false,
            _ => {}
        }
    }
    shown
}

/// The agent tab list of project tab `tab_id`, as `warden:agents` carries it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentListDto {
    pub tab_id: String,
    /// Every tab not closed, in list order.
    pub rows: Vec<AgentRowDto>,
    /// The groups holding a row, in list order.
    pub groups: Vec<AgentGroupDto>,
    /// The closed fold, most recently closed first.
    pub closed: Vec<ClosedAgentDto>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRowDto {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    /// The tab it was forked from, as the list nests it ([`agents::parent_of`]).
    pub parent: Option<String>,
    /// How many parents up it sits from a top-level tab ([`agents::depth_of`]).
    pub depth: u32,
    /// How many rows nest under it.
    pub forks: u32,
    /// Its forks are folded away.
    pub folded: bool,
    /// Under a folded tab.
    pub hidden: bool,
    /// Folded, with a row under it needing you.
    pub amber: bool,
    /// `working`, `blocked`, `done`, `error`, `idle` or `asleep`.
    pub state: &'static str,
    /// What a `blocked` row waits on: `permission`, `question` or `auth`.
    pub blocked_kind: Option<&'static str>,
    /// The program's status message, else its terminal title.
    pub context: Option<String>,
    /// The summary block's rows for a live tab: the user's latest prompt and any short reply
    /// typed since it, the program's status message (its current step), the agent's latest
    /// reply, the conversation's topic, and what the agent waits on the user for — only while
    /// it isn't working, so a turn under way never shows the last turn's question.
    pub asked: Option<String>,
    pub answered: Option<String>,
    pub doing: Option<String>,
    pub said: Option<String>,
    pub topic: Option<String>,
    pub needs: Option<String>,
    /// The background work still running as the agent's last turn ended ("2 shells · 1
    /// agent"), while it isn't working.
    pub background: Option<String>,
    /// When the conversation's prompt cache goes cold, in epoch seconds, while it isn't
    /// working.
    pub cache: Option<u64>,
    pub selected: bool,
    pub can_sleep: bool,
    /// Why a live tab can't sleep.
    pub sleep_reason: Option<String>,
    pub can_fork: bool,
    /// Why the tab can't fork ([`refusal`] of [`AgentOp::Fork`]).
    pub fork_reason: Option<String>,
    /// Done or errored since its tab was last on screen ([`Attention::needs_you`]).
    pub unread: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentGroupDto {
    pub name: String,
    pub folded: bool,
    pub amber: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedAgentDto {
    pub id: String,
    pub name: String,
    pub closed_at: u64,
}

/// A step through an agent tab list, as the menu's Previous / Next and Move Up / Down take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Prev,
    Next,
}

/// The rows `list` shows: every row but those inside a folded group or under a folded tab.
fn visible_rows(list: &AgentListDto) -> Vec<&AgentRowDto> {
    let folded = |g: &Option<String>| {
        g.as_ref()
            .is_some_and(|g| list.groups.iter().any(|x| x.name == *g && x.folded))
    };
    list.rows
        .iter()
        .filter(|r| !r.hidden && !folded(&r.group))
        .collect()
}

/// The record id of the shown row before or after the selected one, wrapping at the ends, as
/// Next / Previous Tab do; `None` with fewer than two shown rows or none of them selected.
pub fn agent_step(list: &AgentListDto, step: Step) -> Option<String> {
    let rows = visible_rows(list);
    if rows.len() < 2 {
        return None;
    }
    let at = rows.iter().position(|r| r.selected)?;
    let to = match step {
        Step::Next => (at + 1) % rows.len(),
        Step::Prev => (at + rows.len() - 1) % rows.len(),
    };
    Some(rows[to].id.clone())
}

/// The selected row moved one place among its siblings, with its forks, as
/// [`agents::Place::Sibling`] takes it: `(record id, index among its other siblings)`. Its
/// siblings are its source's other forks, or for a top-level tab its group's other top-level
/// tabs. `None` at their edge (leaving them is a drag), or with nothing selected.
pub fn agent_move(list: &AgentListDto, step: Step) -> Option<(String, usize)> {
    let sel = list.rows.iter().find(|r| r.selected)?;
    let members: Vec<&AgentRowDto> = list
        .rows
        .iter()
        .filter(|r| r.parent == sel.parent && (sel.parent.is_some() || r.group == sel.group))
        .collect();
    let at = members.iter().position(|r| r.id == sel.id)?;
    let to = match step {
        Step::Prev => at.checked_sub(1)?,
        Step::Next => Some(at + 1).filter(|&i| i < members.len())?,
    };
    Some((sel.id.clone(), to))
}

/// Project `tab_id`'s list from `store`, a live row's state and context from `status` (the hub's
/// [`RexHub::block_status`]), and a group amber when a row in it needs you ([`Attention`]).
pub fn agent_list(
    tab_id: &str,
    store: &AgentStore,
    attention: &Attention,
    status: impl Fn(&str) -> Option<BlockStatus>,
) -> AgentListDto {
    let records = agents::in_list_order(&store.records(tab_id));
    let folded = store.folded(tab_id);
    let mut rows = Vec::new();
    let mut groups: Vec<AgentGroupDto> = Vec::new();
    for r in records.iter().filter(|r| r.state != RecordState::Closed) {
        let live = r.state == RecordState::Live;
        let st = r.block_id.as_deref().filter(|_| live).and_then(&status);
        let state = match (&r.state, st.as_ref().map(|s| s.state)) {
            (RecordState::Live, Some(AgentState::Working)) => "working",
            (RecordState::Live, Some(AgentState::Blocked)) => "blocked",
            (RecordState::Live, Some(AgentState::Done)) => "done",
            (RecordState::Live, Some(AgentState::Error)) => "error",
            (RecordState::Live, _) => "idle",
            _ => "asleep",
        };
        let needs_you = match (r.block_id.as_deref(), st.as_ref()) {
            (Some(block), Some(s)) if live => attention.needs_you(block, s.state),
            _ => false,
        };
        if let Some(g) = &r.group {
            match groups.iter_mut().find(|x| x.name == *g) {
                Some(group) => group.amber |= needs_you,
                None => groups.push(AgentGroupDto {
                    name: g.clone(),
                    folded: folded.contains(g),
                    amber: needs_you,
                }),
            }
        }
        let fork_reason = refusal(&AgentOp::Fork(r.id.clone()), std::slice::from_ref(r));
        let st = st.unwrap_or(BlockStatus {
            state: AgentState::Idle,
            kind: None,
            msg: None,
            title: None,
            asked: None,
            answered: None,
            said: None,
            topic: None,
            needs: None,
            background: None,
            cache: None,
        });
        rows.push(AgentRowDto {
            id: r.id.clone(),
            name: r.name.clone(),
            group: r.group.clone(),
            parent: agents::parent_of(&records, r),
            depth: agents::depth_of(&records, r),
            forks: 0,
            folded: r.folded,
            hidden: false,
            amber: needs_you,
            state,
            blocked_kind: st.kind.filter(|_| state == "blocked").map(|k| match k {
                BlockedKind::Permission => "permission",
                BlockedKind::Question => "question",
                BlockedKind::Auth => "auth",
            }),
            context: st.msg.clone().or(st.title),
            asked: st.asked,
            answered: st.answered,
            doing: st.msg,
            said: st.said,
            topic: st.topic,
            needs: st.needs.filter(|_| state != "working"),
            background: st.background.filter(|_| state != "working"),
            cache: st
                .cache
                .and_then(|c| c.trim().parse().ok())
                .filter(|_| state != "working"),
            selected: r.selected,
            can_sleep: live && r.resume.is_some(),
            sleep_reason: (live && r.resume.is_none()).then(|| NO_RESUME.to_owned()),
            can_fork: fork_reason.is_none(),
            fork_reason: fork_reason.map(str::to_owned),
            unread: needs_you && matches!(state, "done" | "error"),
        });
    }
    // Rows are depth first, so a row's forks are the rows after it deeper than it. `amber` held
    // each row's own need until here.
    let needs: Vec<bool> = rows.iter().map(|r| r.amber).collect();
    for i in 0..rows.len() {
        let end = (i + 1..rows.len())
            .find(|&j| rows[j].depth <= rows[i].depth)
            .unwrap_or(rows.len());
        let r = &mut rows[i];
        r.forks = (end - i - 1) as u32;
        r.folded &= r.forks > 0;
        r.amber = r.folded && needs[i + 1..end].contains(&true);
        if r.folded {
            rows[i + 1..end].iter_mut().for_each(|k| k.hidden = true);
        }
    }
    let mut closed: Vec<&AgentRecord> = records
        .iter()
        .filter(|r| r.state == RecordState::Closed)
        .collect();
    closed.sort_by_key(|r| std::cmp::Reverse(r.closed_at));
    AgentListDto {
        tab_id: tab_id.to_owned(),
        rows,
        groups,
        closed: closed
            .into_iter()
            .map(|r| ClosedAgentDto {
                id: r.id.clone(),
                name: r.name.clone(),
                closed_at: r.closed_at.unwrap_or(0),
            })
            .collect(),
    }
}

/// Send project `tab_id`'s list (`warden:agents`) to every window. The list is the same in any
/// window holding the tab, and a window without it ignores it, so it goes unaddressed.
pub fn emit_agents(app: &AppHandle, tab_id: &str) {
    if let Some(dto) = current_list(app, tab_id) {
        let _ = app.emit("warden:agents", dto);
    }
    crate::sync_agent_menu(app);
}

/// What the agent tab menu items can do for tab `tab_id`: `(sleep, reopen)` — its selected agent
/// tab is live and can sleep; it has a closed agent tab to reopen.
pub fn menu_actions(app: &AppHandle, tab_id: &str) -> (bool, bool) {
    let Some(store) = app.try_state::<Mutex<AgentStore>>() else {
        return (false, false);
    };
    let records = lock(&store).records(tab_id);
    let sleep = records
        .iter()
        .any(|r| r.selected && r.state == RecordState::Live && r.resume.is_some());
    let reopen = records.iter().any(|r| r.state == RecordState::Closed);
    (sleep, reopen)
}

/// Project `tab_id`'s list as it stands: what `warden:agents` carries, and what the chrome pulls
/// (`agents_init`) for a tab it has no list for yet.
pub fn current_list(app: &AppHandle, tab_id: &str) -> Option<AgentListDto> {
    let store = app.try_state::<Mutex<AgentStore>>()?;
    let hub = app.try_state::<RexHub>()?;
    let attention = app.try_state::<Mutex<Attention>>()?;
    let dto = agent_list(tab_id, &lock(&store), &lock(&attention), |b| {
        hub.block_status(b)
    });
    Some(dto)
}

/// Sleep Rex server `server` from window `label` (main thread). A worker plans it
/// ([`RexHub::plan_server_sleep`], which reads Rex). With a loss not among `accepted` (the names
/// the user confirmed), nothing happens yet: window `label` is asked to confirm
/// (`warden:sleep-confirm`, every loss by name) and answers with this again. Otherwise
/// every pane on the server lets go of its surface ([`let_go`]) and a worker sleeps the server
/// ([`RexHub::sleep_server`]). A failure goes to `label`'s error banner; after the sleep the
/// server's agent lists and presence repaint.
pub fn sleep_server(
    app: &AppHandle,
    label: &str,
    server: &str,
    accepted: &[String],
) -> Result<(), String> {
    let hub = app.try_state::<RexHub>().ok_or("Rex isn't available")?;
    hub.stoppable(server)
        .map_err(|why| format!("Rex server {server}: {why}"))?;
    let titles: std::collections::HashMap<String, String> = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows
            .values()
            .flat_map(|ws| {
                ws.registry
                    .rex_tabs()
                    .into_iter()
                    .filter_map(|(id, _)| Some((id.clone(), ws.registry.tab_title(&id)?)))
            })
            .collect()
    };
    let (app, label, server) = (app.clone(), label.to_owned(), server.to_owned());
    let accepted = accepted.to_vec();
    worker("warden-server-sleep", move || {
        let planned = match (
            app.try_state::<RexHub>(),
            app.try_state::<Mutex<AgentStore>>(),
        ) {
            (Some(hub), Some(store)) => {
                hub.plan_server_sleep(&store, &server, &|k| titles.get(k).cloned())
            }
            _ => Err(RexError::Unreachable),
        };
        let main = app.clone();
        let _ = app.run_on_main_thread(move || match planned {
            Err(e) => fail(
                &main,
                &label,
                &format!("couldn't sleep Rex server {server}: {e}"),
            ),
            Ok(plan) if !plan.unconfirmed(&accepted).is_empty() => {
                confirm(&main, &label, Asleep::Server(&server), &plan)
            }
            Ok(plan) => {
                let projects = let_go(&main, |_, s| s == server);
                stop_server(&main, label, server, plan, titles, projects);
            }
        });
    })
}

/// Sleep project tab `tab_id` of window `label` (main thread): Sleep Server's flow narrowed to
/// one tab — plan on a worker ([`RexHub::plan_project_sleep`]); a loss not among `accepted` asks
/// `label` to confirm and nothing happens yet; otherwise the tab lets go of its surfaces in every
/// window ([`let_go`]) and a worker sleeps it ([`RexHub::sleep_project`]), then its list and dot
/// repaint (the ghost, while it holds asleep agents). A failure goes to `label`'s error banner.
pub fn sleep_project(
    app: &AppHandle,
    label: &str,
    tab_id: &str,
    accepted: &[String],
) -> Result<(), String> {
    let (server, panes, title) = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        let reg = &m.windows.get(label).ok_or(NO_AGENT)?.registry;
        let title = reg.tab_title(tab_id).unwrap_or_else(|| tab_id.to_owned());
        match reg.end_target(tab_id, crate::registry::EndSession::Suspend) {
            Some(crate::registry::EndTarget::RexSleep { server, panes }) => (server, panes, title),
            _ => return Err("that tab isn't on a Rex server".into()),
        }
    };
    let (app, label, tab_id) = (app.clone(), label.to_owned(), tab_id.to_owned());
    let accepted = accepted.to_vec();
    worker("warden-project-sleep", move || {
        let named = |k: &str| (k == tab_id).then(|| title.clone());
        let planned = match (
            app.try_state::<RexHub>(),
            app.try_state::<Mutex<AgentStore>>(),
        ) {
            (Some(hub), Some(store)) => hub.plan_project_sleep(&store, &server, &panes, &named),
            _ => Err(RexError::Unreachable),
        };
        let main = app.clone();
        let _ = app.run_on_main_thread(move || {
            let what = Asleep::Project {
                tab_id: &tab_id,
                title: &title,
            };
            match planned {
                Err(e) => fail(&main, &label, &format!("couldn't sleep {title}: {e}")),
                Ok(plan) if !plan.unconfirmed(&accepted).is_empty() => {
                    confirm(&main, &label, what, &plan)
                }
                Ok(plan) => {
                    let_go(&main, |id, _| id == tab_id);
                    end_project(&main, label, tab_id, title, server, panes, plan);
                }
            }
        });
    })
}

/// Sleep project `tab_id` as `plan` says on a worker, then repaint its list, its presence and the
/// agent menu (main thread). A failure goes to window `label`'s banner; a loss found just before
/// its sessions end asks `label` to confirm again.
fn end_project(
    app: &AppHandle,
    label: String,
    tab_id: String,
    title: String,
    server: String,
    panes: Vec<String>,
    plan: SleepPlan,
) {
    let started = worker("warden-project-end", {
        let (app, label) = (app.clone(), label.clone());
        move || {
            let named = |k: &str| (k == tab_id).then(|| title.clone());
            let result = match (
                app.try_state::<RexHub>(),
                app.try_state::<Mutex<AgentStore>>(),
            ) {
                (Some(hub), Some(store)) => {
                    hub.sleep_project(&store, &server, &panes, &plan, &named)
                }
                _ => Err(RexError::Unreachable),
            };
            let main = app.clone();
            let _ = app.run_on_main_thread(move || {
                match result {
                    Ok(Slept::Done) => {}
                    Ok(Slept::Unconfirmed(now)) => confirm(
                        &main,
                        &label,
                        Asleep::Project {
                            tab_id: &tab_id,
                            title: &title,
                        },
                        &now,
                    ),
                    Err(e) => fail(&main, &label, &format!("couldn't sleep {title}: {e}")),
                }
                emit_agents(&main, &tab_id);
                super::spawn::hub_changed(&main, Some(&server));
                crate::sync_agent_menu(&main);
                crate::probe::bump_tab_await(&label, &tab_id, crate::probe::Await::Down);
            });
        }
    });
    if let Err(e) = started {
        fail(app, &label, &e);
    }
}

/// What a sleep confirm answers to: Rex server `server`, or project tab `tab_id` titled `title`.
enum Asleep<'a> {
    Server(&'a str),
    Project { tab_id: &'a str, title: &'a str },
}

/// Ask window `label` to confirm a sleep, listing `plan`'s losses (main thread). The chrome
/// answers with `server_sleep` or `project_sleep`, whichever key the payload carries.
fn confirm(app: &AppHandle, label: &str, what: Asleep, plan: &SleepPlan) {
    let payload = match what {
        Asleep::Server(server) => serde_json::json!({
            "label": label, "server": server, "losses": plan.lose,
            "head": format!("Sleep Rex server {server}?"),
        }),
        Asleep::Project { tab_id, title } => serde_json::json!({
            "label": label, "tabId": tab_id, "losses": plan.lose,
            "head": format!("Sleep {title}?"),
        }),
    };
    let _ = app.emit_to(label, "warden:sleep-confirm", payload);
}

/// Run `f` on a named worker thread; `Err` says it couldn't start.
fn worker(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(|_| ())
        .map_err(|e| format!("couldn't start {name}: {e}"))
}

/// Log `message` and show it in window `label`'s error banner (main thread).
fn fail(app: &AppHandle, label: &str, message: &str) {
    log::error!("{label}: {message}");
    banner(app, label, message);
}

/// Show `message` in window `label`'s error banner (`warden:error`).
pub(crate) fn banner(app: &AppHandle, label: &str, message: &str) {
    let _ = app.emit_to(
        label,
        "warden:error",
        serde_json::json!({ "label": label, "message": message }),
    );
}

/// Every Rex tab `pick` (tab id, server) chooses lets go of its surfaces (main thread), so no
/// relay sees its stream die and reads it as an exit: every docked one goes cold, as an unload,
/// then a popped-out one ends whole, as when its program exits
/// ([`WindowManager::end_detached`](crate::manager::WindowManager::end_detached)). Returns the tabs, for their lists to repaint.
pub(super) fn let_go(app: &AppHandle, pick: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let st = app.state::<ManagerState>();
    let mut projects = Vec::new();
    let mut exited = Vec::new();
    let mut ended = Vec::new();
    {
        let mut m = st.lock();
        // Docked tabs first: a pop-out ending while its origin still runs a sibling agent shows
        // that sibling (`DetachedEnded::SiblingShown`), whose open could restart Rex after the
        // stop. Unloaded first, the origin holds nothing live and the pop-out ends it cold.
        let labels: Vec<String> = m.windows.keys().cloned().collect();
        for l in labels {
            let Some(ws) = m.windows.get_mut(&l) else {
                continue;
            };
            for (id, _) in ws
                .registry
                .rex_tabs()
                .into_iter()
                .filter(|(id, s)| pick(id, s))
            {
                let new_active = ws.registry.unload(&id);
                exited.push((l.clone(), id.clone(), new_active));
                projects.push(id);
            }
            m.record_session(app, &l);
        }
        let popped: Vec<String> = m
            .detached
            .iter()
            .filter(|(_, d)| d.spec.server.as_deref().is_some_and(|s| pick(&d.tab_id, s)))
            .map(|(l, _)| l.clone())
            .collect();
        for dlabel in popped {
            ended.extend(m.end_detached(app, &dlabel));
        }
    }
    for (l, id, new_active) in exited {
        let _ = app.emit_to(
            l.as_str(),
            "warden:tab-exited",
            serde_json::json!({ "label": l, "id": id, "newActive": new_active }),
        );
    }
    for e in ended {
        crate::manager::announce_detached_ended(app, e);
    }
    crate::sync_agent_menu(app);
    projects.sort();
    projects.dedup();
    projects
}

/// Sleep `server` as `plan` says on a worker, then repaint `projects`' lists, the server's
/// presence and the Sleep Server menu (main thread). A failure goes to window `label`'s banner;
/// a loss found just before the stop asks `label` to confirm again.
fn stop_server(
    app: &AppHandle,
    label: String,
    server: String,
    plan: SleepPlan,
    titles: std::collections::HashMap<String, String>,
    projects: Vec<String>,
) {
    let app = app.clone();
    let started = worker("warden-server-stop", {
        let (app, label) = (app.clone(), label.clone());
        move || {
            let result = match (
                app.try_state::<RexHub>(),
                app.try_state::<Mutex<AgentStore>>(),
            ) {
                (Some(hub), Some(store)) => {
                    hub.sleep_server(&store, &server, &plan, &|k| titles.get(k).cloned())
                }
                _ => Err(RexError::Unreachable),
            };
            let main = app.clone();
            let _ = app.run_on_main_thread(move || {
                match result {
                    Ok(Slept::Done) => {}
                    Ok(Slept::Unconfirmed(now)) => {
                        confirm(&main, &label, Asleep::Server(&server), &now)
                    }
                    Err(e) => fail(
                        &main,
                        &label,
                        &format!("couldn't sleep Rex server {server}: {e}"),
                    ),
                }
                for p in &projects {
                    emit_agents(&main, p);
                }
                super::spawn::hub_changed(&main, Some(&server));
                crate::sync_server_menu(&main);
            });
        }
    });
    if let Err(e) = started {
        fail(&app, &label, &e);
    }
}

/// Run `op` on tab `tab_id`'s agent pane for window `label` (main thread). A refusal answers at
/// once; the Rex work runs on a worker and lands through [`land`]. A live agent sleeping or
/// closing lets its surface go first, so its window closing never reads as the agent exiting.
pub fn run(app: &AppHandle, label: &str, tab_id: &str, op: AgentOp) -> Result<(), String> {
    let store = app
        .try_state::<Mutex<AgentStore>>()
        .ok_or("agent tabs aren't available")?;
    let req = {
        let st = app.state::<ManagerState>();
        let mut m = st.lock();
        let ws = m.windows.get_mut(label).ok_or("window not found")?;
        let req = ws
            .registry
            .agent_request(tab_id)
            .ok_or("this tab has no agent tabs")?;
        if let Some(why) = refusal(&op, &lock(&store).records(tab_id)) {
            return Err(why.to_owned());
        }
        log::info!("tab {tab_id:?}: agent {op:?}");
        if let AgentOp::Sleep(id) | AgentOp::Close { id, .. } = &op {
            ws.registry.release_agent(tab_id, id);
        }
        req
    };
    // The agent tab going away ends its program, and that report must not read as it finishing.
    let dismissed = match &op {
        AgentOp::Sleep(id) | AgentOp::Close { id, .. } => lock(&store)
            .records(tab_id)
            .into_iter()
            .find(|r| r.id == *id)
            .and_then(|r| r.block_id),
        _ => None,
    };
    if let (Some(block), Some(attention)) = (&dismissed, app.try_state::<Mutex<Attention>>()) {
        lock(&attention).dismiss(block);
    }
    let (app, label) = (app.clone(), label.to_owned());
    let focus_main = matches!(op, AgentOp::New);
    std::thread::Builder::new()
        .name("warden-agent-op".into())
        .spawn(move || {
            let land_on_main = |result: Result<AgentsOpened, String>| {
                let (main, label, pane) = (app.clone(), label.clone(), req.pane_key.clone());
                let _ = app.run_on_main_thread(move || {
                    land(&main, Some(&label), &pane, result, focus_main)
                });
            };
            let done = match (
                app.try_state::<RexHub>(),
                app.try_state::<Mutex<AgentStore>>(),
            ) {
                (Some(hub), Some(store)) => {
                    hub.agent_op_shown(&store, &req, &op, unix_now(), |o| land_on_main(Ok(o)))
                }
                _ => Err(format!("Rex server {}: no connection to Rex", req.server)),
            };
            if let Err(message) = done {
                if let (Some(block), Some(attention)) =
                    (&dismissed, app.try_state::<Mutex<Attention>>())
                {
                    lock(&attention).restore(block);
                }
                land_on_main(Err(message));
                // The surface went before the worker started; a refresh re-spawns the agent the
                // failed sleep or close left selected.
                if matches!(op, AgentOp::Sleep(_) | AgentOp::Close { .. }) {
                    follow(&app, &req.pane_key, &AgentOp::Refresh);
                }
            }
            // A wake can create the pane's session and a sleep or close end it: repaint the
            // presence dot, as an open does (`spawn::opener`).
            super::spawn::hub_changed(&app, Some(&req.server));
        })
        .map(|_| ())
        .map_err(|e| format!("couldn't start the agent tab change: {e}"))
}

/// Select agent `record_id` of tab `tab_id` from window `label` (main thread): an asleep one
/// wakes ([`AgentOp::Wake`]); a live one becomes the project's one selected record and shows. A
/// pane not yet open is opened as the user's, which shows the selected agent.
pub fn select(app: &AppHandle, label: &str, tab_id: &str, record_id: &str) -> Result<(), String> {
    let store = app
        .try_state::<Mutex<AgentStore>>()
        .ok_or("agent tabs aren't available")?;
    let state = lock(&store)
        .records(tab_id)
        .into_iter()
        .find(|r| r.id == record_id)
        .map(|r| r.state)
        .ok_or(NO_AGENT)?;
    match state {
        RecordState::Asleep => return run(app, label, tab_id, AgentOp::Wake(record_id.into())),
        RecordState::Closed => return Err(NO_AGENT.to_owned()),
        RecordState::Live => {}
    }
    lock(&store).select(tab_id, record_id);
    let st = app.state::<ManagerState>();
    let mut m = st.lock();
    let ws = m.windows.get_mut(label).ok_or("window not found")?;
    let shown = match ws.registry.select_agent(tab_id, record_id) {
        Ok(false) if ws.registry.active_tab() == Some(tab_id) => {
            ws.registry.activate_user(tab_id).map(|_| ())
        }
        r => r.map(|_| ()),
    };
    m.record_session(app, label);
    let dto = m.init_dto(label);
    drop(m);
    if let Some(dto) = dto {
        let _ = app.emit_to(label, "warden:refresh", dto);
    }
    emit_agents(app, tab_id);
    shown.map_err(|e| format!("couldn't open terminal: {e}"))
}

/// Change tab `tab_id`'s list in the store alone (groups, folds, the closed fold) and re-send it.
/// `edit` answers whether the record it names exists.
pub fn edit(
    app: &AppHandle,
    tab_id: &str,
    edit: impl FnOnce(&mut AgentStore) -> bool,
) -> Result<(), String> {
    let store = app
        .try_state::<Mutex<AgentStore>>()
        .ok_or("agent tabs aren't available")?;
    let found = edit(&mut lock(&store));
    emit_agents(app, tab_id);
    found.then_some(()).ok_or_else(|| NO_AGENT.to_owned())
}

/// Land an op's answer for project `tab_id` (main thread): every window holding the tab follows
/// it and repaints; a failure goes to window `label`'s error banner. Then the list. With
/// `focus_main` (a new agent tab), window `label`'s main pane takes keyboard focus, so the agent
/// it shows is typed into whichever pane ⌘T or `+` came from.
fn land(
    app: &AppHandle,
    label: Option<&str>,
    tab_id: &str,
    result: Result<AgentsOpened, String>,
    focus_main: bool,
) {
    let st = app.state::<ManagerState>();
    let mut m = st.lock();
    let mut refreshed = Vec::new();
    if let Ok(found) = &result {
        for (l, ws) in m.windows.iter_mut() {
            if ws.registry.spec_of(tab_id).is_none() {
                continue;
            }
            // Before the sync, whose show focuses the pane the tab has focused.
            if focus_main && label == Some(l.as_str()) {
                ws.registry.focus_pane(tab_id, PaneIdx::Primary);
            }
            if let Err(e) = ws.registry.sync_agents(tab_id, found.clone()) {
                log::error!("couldn't open an agent of tab {tab_id:?}: {e}");
            }
            refreshed.push(l.clone());
        }
    }
    let dtos: Vec<_> = refreshed
        .iter()
        .filter_map(|l| {
            m.record_session(app, l);
            Some((l.clone(), m.init_dto(l)?))
        })
        .collect();
    drop(m);
    for (l, dto) in dtos {
        let _ = app.emit_to(l.as_str(), "warden:refresh", dto);
    }
    if let (Err(message), Some(label)) = (&result, label) {
        log::error!("{label}: {tab_id}: {message}");
        banner(app, label, message);
    }
    emit_agents(app, tab_id);
}

/// Follow Rex for the app's life: a followed pane's windows changing ([`RexHub::agent_panes`])
/// reconciles its list on a worker ([`AgentOp::Refresh`]) and lands it; a block's status
/// changing is [`attend`]ed on the main thread.
pub fn watch_agents(app: AppHandle) {
    let Some(hub) = app.try_state::<RexHub>().map(|h| h.inner().clone()) else {
        return;
    };
    let events = hub.subscribe();
    let spawned = std::thread::Builder::new()
        .name("warden-agent-tabs".into())
        .spawn(move || {
            for ev in events {
                match ev {
                    HubEvent::WindowsChanged { server, session_id } => {
                        for project in hub.agent_panes(&server, &session_id) {
                            refresh(&app, project);
                        }
                        if let Some(opened) = hub.column(&server, &session_id) {
                            let main = app.clone();
                            let _ = app.run_on_main_thread(move || {
                                follow_column(&main, &server, &session_id, opened)
                            });
                        }
                    }
                    HubEvent::StatusChanged { block_id } => {
                        let main = app.clone();
                        let _ = app.run_on_main_thread(move || attend(&main, &block_id));
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        log::error!("agent tabs won't follow Rex: {e}");
    }
}

/// `server`'s session `session_id` changed its windows (main thread): every tab whose scratch
/// column it is follows it ([`crate::registry::Registry::sync_column`]) and its window's chrome
/// repaints.
fn follow_column(
    app: &AppHandle,
    server: &str,
    session_id: &str,
    opened: super::column::ColumnOpened,
) {
    let st = app.state::<ManagerState>();
    let mut m = st.lock();
    let mut changed = Vec::new();
    let mut empty = None;
    for (label, ws) in m.windows.iter_mut() {
        let tabs = ws.registry.columns_on(server, session_id);
        for id in &tabs {
            ws.registry.sync_column(id, opened.clone());
            empty = empty.or_else(|| ws.registry.empty_rex_column(id));
        }
        if !tabs.is_empty() {
            changed.push(label.clone());
        }
    }
    let dtos: Vec<_> = changed
        .into_iter()
        .filter_map(|label| {
            m.record_session(app, &label);
            Some((m.init_dto(&label)?, label))
        })
        .collect();
    drop(m);
    let any = !dtos.is_empty();
    for (dto, label) in dtos {
        let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
    }
    // Pop Out's enabled state follows the column's panes.
    if any {
        crate::sync_agent_menu(app);
    }
    if let Some((key, dir, stack)) = empty {
        fill_column(app, key, dir, stack);
    }
}

/// Column `key`'s session lost its last scratch tab: give it a fresh one in `dir`, laid out as
/// the tab's configured `stack`, on a worker ([`RexHub::column_fill`]); the session's next view
/// brings it in.
fn fill_column(
    app: &AppHandle,
    key: String,
    dir: std::path::PathBuf,
    stack: Vec<warden_config::StackPane>,
) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("warden-rex-fill".into())
        .spawn(move || {
            if let Some(hub) = app.try_state::<RexHub>() {
                if let Err(e) = hub.column_fill(&key, &dir, &stack) {
                    log::error!("couldn't open a fresh scratch tab in {key:?}: {e}");
                }
            }
        });
    if let Err(e) = spawned {
        log::error!("couldn't start refilling a scratch column: {e}");
    }
}

/// Block `block_id`'s status changed (main thread): track it ([`Attention::observe`]) and, for an
/// agent tab, re-send its project's list. A hidden agent tab that now needs you raises attention
/// on its project's row, with a banner naming the agent tab and saying why
/// ([`crate::notify::raise`]); clicking the banner selects it.
fn attend(app: &AppHandle, block_id: &str) {
    let (Some(store), Some(hub), Some(attention)) = (
        app.try_state::<Mutex<AgentStore>>(),
        app.try_state::<RexHub>(),
        app.try_state::<Mutex<Attention>>(),
    ) else {
        return;
    };
    let status = hub.block_status(block_id);
    let state = status.as_ref().map_or(AgentState::Idle, |s| s.state);
    let record = {
        let s = lock(&store);
        s.project_of_block(block_id).and_then(|p| {
            s.records(&p)
                .into_iter()
                .find(|r| r.block_id.as_deref() == Some(block_id))
        })
    };
    let Some(record) = record else {
        let mut a = lock(&attention);
        match status {
            Some(_) => {
                a.observe(block_id, state, false);
            }
            None => a.forget(block_id),
        }
        return;
    };
    let visible = visible_agents(app)
        .iter()
        .any(|(p, id)| *p == record.project && *id == record.id);
    if lock(&attention).observe(block_id, state, visible) {
        let tab = {
            let st = app.state::<ManagerState>();
            let m = st.lock();
            m.windows.iter().find_map(|(label, ws)| {
                Some((label.clone(), ws.registry.tab_title(&record.project)?))
            })
        };
        if let Some((label, title)) = tab {
            let why = match state {
                AgentState::Blocked => "blocked",
                AgentState::Error => "error",
                _ => "done",
            };
            let lock = status
                .as_ref()
                .is_some_and(|s| s.kind == Some(BlockedKind::Permission));
            let body = status.and_then(|s| s.msg).unwrap_or_else(|| why.to_owned());
            crate::notify::raise(
                app,
                &label,
                &record.project,
                false,
                Some((format!("{title} · {}", record.name), body)),
                Some(&record.id),
                lock,
            );
        }
    }
    emit_agents(app, &record.project);
}

/// The agent tabs on screen, as `(project, record id)`: each focused window's active tab's
/// selected agent tab, and each focused pop-out's travelling one.
fn visible_agents(app: &AppHandle) -> Vec<(String, String)> {
    let focused: Vec<String> = app
        .windows()
        .into_iter()
        .filter(|(_, w)| w.is_focused().unwrap_or(false))
        .map(|(label, _)| label)
        .collect();
    if focused.is_empty() {
        return Vec::new();
    }
    let st = app.state::<ManagerState>();
    let m = st.lock();
    let docked = m
        .windows
        .iter()
        .filter(|(label, _)| focused.contains(label))
        .filter_map(|(_, ws)| {
            let tab = ws.registry.active_tab()?;
            Some((tab.to_owned(), ws.registry.visible_agent(tab)?.to_owned()))
        });
    let popped = m
        .detached
        .iter()
        .filter(|(label, _)| focused.contains(label))
        .filter_map(|(_, ds)| Some((ds.tab_id.clone(), ds.agent.clone()?)));
    docked.chain(popped).collect()
}

/// Mark the agent tabs on screen seen ([`Attention::seen`]), re-sending the list of each project
/// that clears for. Locks `ManagerState` and the stores, so callers must hold none.
pub fn mark_seen(app: &AppHandle) {
    let (Some(store), Some(attention)) = (
        app.try_state::<Mutex<AgentStore>>(),
        app.try_state::<Mutex<Attention>>(),
    ) else {
        return;
    };
    let visible = visible_agents(app);
    if visible.is_empty() {
        return;
    }
    let blocks: Vec<(String, String)> = {
        let s = lock(&store);
        visible
            .into_iter()
            .filter_map(|(project, id)| {
                let block = s
                    .records(&project)
                    .into_iter()
                    .find(|r| r.id == id)?
                    .block_id?;
                Some((project, block))
            })
            .collect()
    };
    let cleared: Vec<String> = {
        let mut a = lock(&attention);
        blocks
            .into_iter()
            .filter(|(_, block)| a.seen(block))
            .map(|(project, _)| project)
            .collect()
    };
    for project in cleared {
        if let Some(dto) = current_list(app, &project) {
            let _ = app.emit("warden:agents", dto);
        }
    }
}

/// Reconcile `project`'s list with its session's windows on a worker ([`follow`]).
fn refresh(app: &AppHandle, project: String) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("warden-agent-refresh".into())
        .spawn(move || follow(&app, &project, &AgentOp::Refresh));
    if let Err(e) = spawned {
        log::error!("couldn't start following agent tabs: {e}");
    }
}

/// Run `op`, one Rex has already made happen, on `project`'s agent pane and land it, if a window
/// holds the tab. Worker only: it waits on the pane's hold and on Rex. A failure is logged, since
/// no window asked for it.
pub(super) fn follow(app: &AppHandle, project: &str, op: &AgentOp) {
    let req = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows
            .values()
            .find_map(|ws| ws.registry.agent_request(project))
    };
    let (Some(req), Some(hub), Some(store)) = (
        req,
        app.try_state::<RexHub>(),
        app.try_state::<Mutex<AgentStore>>(),
    ) else {
        return;
    };
    let result = hub.agent_op(&store, &req, op, unix_now());
    if let Err(e) = &result {
        log::error!("couldn't follow {project:?}'s agent tabs: {e}");
        return;
    }
    let (main, project) = (app.clone(), project.to_owned());
    let _ = app.run_on_main_thread(move || land(&main, None, &project, result, false));
}

/// Listen for the resume commands relays report ([`super::reports`]) for the app's life, storing
/// each on its record ([`AgentStore::report_resume`]) and re-sending that project's list. A
/// socket that can't be bound ([`super::reports::socket_path`]) leaves reports dropped; the app
/// runs on.
pub fn watch_reports(app: AppHandle) {
    let Some(path) = super::reports::socket_path(&app) else {
        log::error!("no app config dir, so agents can't report how to resume");
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let reports = match super::reports::listen(&path) {
        Ok(rx) => rx,
        Err(e) => {
            log::error!(
                "agents can't report how to resume ({}): {e}",
                path.display()
            );
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("warden-agent-resume".into())
        .spawn(move || {
            for report in reports {
                let Some(store) = app.try_state::<Mutex<AgentStore>>() else {
                    return;
                };
                let project = {
                    let mut s = lock(&store);
                    s.report_resume(&report.block_id, &report.resume)
                        .then(|| s.project_of_block(&report.block_id))
                        .flatten()
                };
                if let Some(project) = project {
                    emit_agents(&app, &project);
                }
            }
        });
    if let Err(e) = spawned {
        log::error!("agents can't report how to resume: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rex_client::fake::Reply;
    use serde_json::json;

    use super::{agent_list, AgentOp, NO_RESUME};
    use crate::rex::agent_open::tests::{fake, hub, map, open, record, state_of, store};
    use crate::rex::agents::RecordState;
    use crate::rex::attention::Attention;

    fn eventually(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn sleep_refused_without_resume() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record("a", RecordState::Live, Some("window:1"), None)]);

        let got = hub.agent_op(&s, &open(None), &AgentOp::Sleep("a".into()), 7);
        assert_eq!(got, Err(NO_RESUME.to_string()));
        assert_eq!(NO_RESUME, "this agent hasn't reported how to resume it");
        assert!(f.requests_for("session.close_window").is_empty());
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Live);
    }

    /// Agent `a` (resumable when `resume`) alone in its session, opened, then its program exits
    /// and its window is let go, ending the session.
    fn exited_alone(
        resume: Option<&str>,
    ) -> (
        rex_client::fake::FakeServer,
        std::sync::Mutex<crate::rex::agents::AgentStore>,
    ) {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        f.reply(
            "session.list_blocks",
            json!({"blocks": [{"block_id": "block:1", "placement": "placed"}]}),
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record("a", RecordState::Live, Some("window:1"), resume);
        a.selected = true;
        let s = store(vec![a]);
        hub.open_agents(&s, &open(None), 7).expect("open");
        f.reply(
            "com.superlogical.terminal.process",
            json!({"last_exit": {"exit_code": 0}}),
        );
        assert!(hub.end_agent_if_exited(&crate::registry::agent_key("t", "a")));
        assert_eq!(f.requests_for("session.destroy").len(), 1);
        hub.agent_op(&s, &open(None), &AgentOp::Exited("a".into()), 9)
            .expect("exited");
        (f, s)
    }

    #[test]
    fn an_exit_that_ends_the_session_closes_the_agent_rather_than_sleeping_it() {
        let (_f, s) = exited_alone(Some("claude --resume aaa"));
        let a = state_of(&s, "a").unwrap();
        assert_eq!(a.state, RecordState::Closed, "its row can't wake it");
        assert_eq!(a.closed_at, Some(9));
        assert_eq!((a.window_id, a.block_id), (None, None));
        assert!(!a.selected);
    }

    #[test]
    fn an_exit_seen_for_an_agent_already_asleep_leaves_it_asleep() {
        // A restored session's open sleeps its agents before closing their windows; a relay
        // still bound to one of those windows then reports its exit.
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record(
            "a",
            RecordState::Asleep,
            None,
            Some("claude --resume aaa"),
        )]);
        hub.agent_op(&s, &open(None), &AgentOp::Exited("a".into()), 9)
            .expect("exited");
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
    }

    #[test]
    fn an_exit_without_a_resume_command_drops_the_agent() {
        let (_f, s) = exited_alone(None);
        assert_eq!(state_of(&s, "a"), None);
    }

    #[test]
    fn sleep_then_wake_types_resume_with_program_swap() {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        );
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);

        let slept = hub
            .agent_op(&s, &open(None), &AgentOp::Sleep("a".into()), 7)
            .expect("sleep");
        let closes = f.requests_for("session.close_window");
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].payload["window_id"], "window:1");
        let a = state_of(&s, "a").unwrap();
        assert_eq!(a.state, RecordState::Asleep);
        assert_eq!((a.window_id, a.block_id), (None, None));
        assert_eq!(
            slept.selected.as_deref(),
            Some("b"),
            "a live neighbour shows"
        );
        assert!(state_of(&s, "b").unwrap().selected);

        let woke = hub
            .agent_op(
                &s,
                &open(Some("claude-work")),
                &AgentOp::Wake("a".into()),
                8,
            )
            .expect("wake");
        let made = f.requests_for("session.new_window");
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].payload["session_id"], "session:old");
        assert_eq!(
            made[0].payload["window_label"], "a",
            "the tab keeps its name"
        );
        assert_eq!(
            made[0].payload["layout"]["block"]["options"]["initial_input"],
            "claude-work --resume aaa\n"
        );
        let a = state_of(&s, "a").unwrap();
        assert_eq!(a.state, RecordState::Live);
        assert_eq!(a.window_id.as_deref(), Some("window:woke"));
        assert!(a.selected && !state_of(&s, "b").unwrap().selected);
        assert_eq!(woke.selected.as_deref(), Some("a"));
        let ids: Vec<&str> = woke.agents.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn close_then_reopen_restores_name_group_position() {
        let f = fake(
            Some("session:old"),
            vec![
                ("window:1", "block:1", "b"),
                ("window:2", "block:2", "alpha"),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record(
            "a",
            RecordState::Live,
            Some("window:2"),
            Some("claude --resume aaa"),
        );
        a.name = "alpha".into();
        a.group = Some("g".into());
        let s = store(vec![
            record("b", RecordState::Live, Some("window:1"), None),
            a,
        ]);

        hub.agent_op(
            &s,
            &open(None),
            &AgentOp::Close {
                id: "a".into(),
                confirmed: false,
            },
            7,
        )
        .expect("close");
        let a = state_of(&s, "a").unwrap();
        assert_eq!(a.state, RecordState::Closed);
        assert_eq!(a.closed_at, Some(7));
        let list = agent_list("t", &s.lock().unwrap(), &Attention::default(), |_| None);
        assert_eq!(list.closed.len(), 1);
        assert_eq!(list.closed[0].name, "alpha");
        assert_eq!(list.closed[0].closed_at, 7);
        assert!(list.rows.iter().all(|r| r.id != "a"), "out of the list");

        hub.agent_op(&s, &open(None), &AgentOp::Reopen(None), 9)
            .expect("reopen");
        let made = f.requests_for("session.new_window");
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].payload["window_label"], "alpha");
        assert_eq!(
            made[0].payload["layout"]["block"]["options"]["initial_input"],
            "claude --resume aaa\n"
        );
        let a = state_of(&s, "a").unwrap();
        assert_eq!(a.state, RecordState::Live);
        assert_eq!(a.closed_at, None);
        assert_eq!((a.name.as_str(), a.group.as_deref()), ("alpha", Some("g")));
        let list = agent_list("t", &s.lock().unwrap(), &Attention::default(), |_| None);
        let ids: Vec<&str> = list.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"], "back where it was");
        assert!(list.closed.is_empty());
    }

    #[test]
    fn close_without_resume_needs_a_confirm() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record("a", RecordState::Live, Some("window:1"), None)]);
        let close = |confirmed| AgentOp::Close {
            id: "a".into(),
            confirmed,
        };

        let got = hub.agent_op(&s, &open(None), &close(false), 7);
        assert_eq!(got, Err(NO_RESUME.to_string()));
        assert!(f.requests_for("session.close_window").is_empty());

        hub.agent_op(&s, &open(None), &close(true), 7)
            .expect("confirmed");
        assert_eq!(f.requests_for("session.close_window").len(), 1);
        assert_eq!(state_of(&s, "a"), None, "gone for good");
    }

    /// The list without the closed tab is stored and shown before Rex answers, which it does
    /// only once the program in the window has exited.
    #[test]
    fn close_shows_before_rex_answers() {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        let slow = Duration::from_millis(600);
        f.on("session.close_window", move |_, _| {
            Reply::ok(json!({})).after(slow)
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        );
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);
        let close = AgentOp::Close {
            id: "a".into(),
            confirmed: false,
        };

        let started = Instant::now();
        let mut shows = Vec::new();
        hub.agent_op_shown(&s, &open(None), &close, 7, |o| {
            shows.push((started.elapsed(), state_of(&s, "a").unwrap().state, o));
        })
        .expect("close");
        assert!(started.elapsed() >= slow, "waited for Rex");
        assert_eq!(shows.len(), 1);
        let (at, state, opened) = &shows[0];
        assert!(*at < slow / 2, "shown at {at:?}, before Rex answered");
        assert_eq!(*state, RecordState::Closed);
        assert_eq!(
            opened.selected.as_deref(),
            Some("b"),
            "leaned to its neighbour"
        );
        assert_eq!(f.requests_for("session.close_window").len(), 1);
    }

    /// A new agent tab doesn't wait for a close still waiting on Rex (its program exiting), and
    /// never takes the closing window for an agent tab of its own.
    #[test]
    fn new_does_not_wait_for_a_closing_window() {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        let slow = Duration::from_millis(800);
        f.on("session.close_window", move |_, _| {
            Reply::ok(json!({})).after(slow)
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        );
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);
        let close = AgentOp::Close {
            id: "a".into(),
            confirmed: false,
        };

        let started = Instant::now();
        let (shown_tx, shown_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let closing = scope.spawn(|| {
                hub.agent_op_shown(&s, &open(None), &close, 7, |_| {
                    let _ = shown_tx.send(());
                })
            });
            shown_rx.recv().expect("the close shows");
            hub.agent_op(&s, &open(None), &AgentOp::New, 8)
                .expect("new");
            assert!(
                started.elapsed() < slow / 2,
                "new done at {:?}, behind the close",
                started.elapsed()
            );
            closing.join().unwrap().expect("close");
        });
        let records = s.lock().unwrap().records("t");
        assert!(
            !records
                .iter()
                .any(|r| r.id != "a" && r.window_id.as_deref() == Some("window:1")),
            "the closing window adopted: {records:?}"
        );
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Closed);
        assert_eq!(records.len(), 3, "a closed, b, and the new tab");
    }

    /// A close Rex refuses puts the record back as it was, shown, and reports the refusal.
    #[test]
    fn refused_close_puts_the_tab_back() {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        f.on("session.close_window", |_, _| {
            Reply::error("internal", "no", 500)
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record("a", RecordState::Live, Some("window:1"), None);
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);
        let close = AgentOp::Close {
            id: "a".into(),
            confirmed: true,
        };

        let mut shows = Vec::new();
        let got = hub.agent_op_shown(&s, &open(None), &close, 7, |o| shows.push(o));
        assert!(got.is_err());
        assert_eq!(shows.len(), 2, "the close, then the tab put back");
        assert_eq!(shows[1].selected.as_deref(), Some("a"));
        let a = state_of(&s, "a").expect("back, though it closes for good");
        assert_eq!(a.state, RecordState::Live);
        assert_eq!(a.window_id.as_deref(), Some("window:1"));
        assert!(a.selected && !state_of(&s, "b").unwrap().selected);
    }

    /// A close Rex is slow to answer (its program slow to exit) was sent: the tab stays closed,
    /// with no error.
    #[test]
    fn timed_out_close_stays_closed() {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        f.on("session.close_window", |_, _| {
            Reply::ok(json!({})).after(Duration::from_secs(12))
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        );
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);
        let close = AgentOp::Close {
            id: "a".into(),
            confirmed: false,
        };

        let mut shows = Vec::new();
        let got = hub.agent_op_shown(&s, &open(None), &close, 7, |o| shows.push(o));
        assert_eq!(got, Ok(()));
        assert_eq!(shows.len(), 1, "only the close shown");
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Closed);
    }

    #[test]
    fn rex_closed_window_moves_to_closed() {
        let f = fake(
            Some("session:old"),
            vec![
                ("window:1", "block:1", "a"),
                ("window:2", "block:2", "b"),
                ("window:3", "block:3", "c"),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut b = record(
            "b",
            RecordState::Live,
            Some("window:2"),
            Some("claude --resume bbb"),
        );
        b.selected = true;
        let s = store(vec![
            record("a", RecordState::Live, Some("window:1"), None),
            b,
            record("c", RecordState::Live, Some("window:3"), None),
        ]);
        hub.agent_op(&s, &open(None), &AgentOp::Refresh, 7)
            .expect("attach");
        assert_eq!(hub.agent_panes("local", "session:old"), ["t"]);

        // Rex.app closes b's window.
        f.push_session_event(
            "session:old",
            "window_closed",
            json!({"window_id": "window:2"}),
        );
        eventually("the closed window to go", || {
            hub.windows("local", "session:old").len() == 2
        });
        let got = hub
            .agent_op(&s, &open(None), &AgentOp::Refresh, 8)
            .expect("refresh");
        let b = state_of(&s, "b").unwrap();
        assert_eq!(b.state, RecordState::Closed);
        assert_eq!(b.closed_at, Some(8));
        assert_eq!(got.selected.as_deref(), Some("a"), "leans up the list");
        assert!(state_of(&s, "a").unwrap().selected && !b.selected);
        let ids: Vec<&str> = got.agents.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
        assert!(f.requests_for("session.new_window").is_empty());
    }

    #[test]
    fn new_agent_is_selected_and_last() {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record("a", RecordState::Live, Some("window:1"), None);
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);

        let got = hub
            .agent_op(&s, &open(None), &AgentOp::New, 7)
            .expect("new");
        let made = f.requests_for("session.new_window");
        assert_eq!(made.len(), 1);
        assert_eq!(
            made[0].payload["layout"]["block"]["options"]["initial_input"], "agent\n",
            "types the tab's cmd"
        );
        assert!(
            made[0].payload.get("window_label").is_none(),
            "Rex names it"
        );
        let list = agent_list("t", &s.lock().unwrap(), &Attention::default(), |_| None);
        assert_eq!(list.rows.len(), 3);
        let last = &list.rows[2];
        assert!(last.selected);
        assert!(list.rows[..2].iter().all(|r| !r.selected));
        assert_eq!(got.selected.as_deref(), Some(last.id.as_str()));
        assert_eq!(got.agents[2].1.block_id, "block:woke");
    }

    type Forkable = (
        rex_client::fake::FakeServer,
        tempfile::TempDir,
        std::sync::Mutex<crate::rex::agents::AgentStore>,
    );

    /// Agents `a` (resumable, selected) and `b` in group `g`, then `c` ungrouped, live in
    /// `session:old`.
    fn forkable() -> Forkable {
        let f = fake(
            Some("session:old"),
            vec![
                ("window:1", "block:1", "a"),
                ("window:2", "block:2", "b"),
                ("window:3", "block:3", "c"),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let mut a = record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        );
        a.selected = true;
        a.group = Some("g".into());
        let mut b = record("b", RecordState::Live, Some("window:2"), None);
        b.group = Some("g".into());
        let c = record("c", RecordState::Live, Some("window:3"), None);
        let s = store(vec![a, b, c]);
        (f, dir, s)
    }

    #[test]
    fn fork_types_the_fork_command_below_the_source() {
        let (f, dir, s) = forkable();
        let (hub, _) = hub(f.endpoint(), &dir);
        let before = state_of(&s, "a").unwrap();

        let got = hub
            .agent_op(&s, &open(None), &AgentOp::Fork("a".into()), 7)
            .expect("fork");
        let made = f.requests_for("session.new_window");
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].payload["session_id"], "session:old");
        assert_eq!(made[0].payload["window_label"], "a fork");
        assert_eq!(
            made[0].payload["layout"]["block"]["options"]["initial_input"],
            "claude --resume aaa --fork-session\n"
        );
        let list = agent_list("t", &s.lock().unwrap(), &Attention::default(), |_| None);
        let names: Vec<&str> = list.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["c", "a", "a fork", "b"],
            "below its source, in its group"
        );
        let fork = &list.rows[2];
        assert_eq!((list.rows[1].depth, fork.depth), (0, 1), "nested under a");
        assert_eq!(fork.group.as_deref(), Some("g"));
        assert!(fork.selected);
        assert_eq!(got.selected.as_deref(), Some(fork.id.as_str()));
        let rec = state_of(&s, &fork.id).unwrap();
        assert_eq!(rec.state, RecordState::Live);
        assert_eq!(rec.resume, None, "it reports its own session");
        assert_eq!(rec.window_id.as_deref(), Some("window:woke"));
        assert_eq!(rec.lineage, ["a"], "forked from a");
        let a = state_of(&s, "a").unwrap();
        assert_eq!(
            (a.state, a.window_id, a.resume),
            (before.state, before.window_id, before.resume),
            "the source is untouched"
        );
        assert!(!a.selected);
    }

    #[test]
    fn forking_a_folded_tab_unfolds_it_so_the_fork_shows() {
        let (f, dir, s) = forkable();
        let mut a = state_of(&s, "a").unwrap();
        a.folded = true;
        s.lock().unwrap().upsert(a);
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.agent_op(&s, &open(None), &AgentOp::Fork("a".into()), 7)
            .expect("fork");
        assert!(!state_of(&s, "a").unwrap().folded);
        let list = agent_list("t", &s.lock().unwrap(), &Attention::default(), |_| None);
        assert!(
            list.rows.iter().all(|r| !r.hidden),
            "the new fork is on screen"
        );
    }

    #[test]
    fn fork_applies_the_configured_resume_program() {
        let (f, dir, s) = forkable();
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.agent_op(
            &s,
            &open(Some("claude-work")),
            &AgentOp::Fork("a".into()),
            7,
        )
        .expect("fork");
        assert_eq!(
            f.requests_for("session.new_window")[0].payload["layout"]["block"]["options"]
                ["initial_input"],
            "claude-work --resume aaa --fork-session\n"
        );
    }

    #[test]
    fn fork_of_an_asleep_agent_in_a_lost_session_creates_it_leaving_the_source_asleep() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record(
            "a",
            RecordState::Asleep,
            None,
            Some("claude --resume aaa"),
        )]);

        let got = hub
            .agent_op(&s, &open(None), &AgentOp::Fork("a".into()), 7)
            .expect("fork");
        let created = f.requests_for("session.create");
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].payload["initial_windows"][0]["layout"]["block"]["options"]["initial_input"],
            "claude --resume aaa --fork-session\n"
        );
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
        assert_eq!(got.agents.len(), 1);
        let recs = s.lock().unwrap().records("t");
        let fork = recs
            .iter()
            .find(|r| r.id != "a")
            .expect("the fork's record");
        assert_eq!(
            (fork.name.as_str(), fork.state),
            ("a fork", RecordState::Live)
        );
        assert_eq!(fork.position, 1, "below its source");
        assert_eq!(got.selected.as_deref(), Some(fork.id.as_str()));
    }

    #[test]
    fn fork_of_a_source_whose_window_just_closed_in_rex_is_refused() {
        // `a`'s window is gone from Rex (closed in Rex.app), so reconcile closes it.
        let f = fake(
            Some("session:old"),
            vec![("window:2", "block:2", "b"), ("window:3", "block:3", "c")],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        )]);
        let got = hub.agent_op(&s, &open(None), &AgentOp::Fork("a".into()), 7);
        assert_eq!(got, Err(super::NO_AGENT.to_string()));
        assert!(f.requests_for("session.new_window").is_empty());
    }

    #[test]
    fn fork_refusals_make_no_window() {
        let (f, dir, s) = forkable();
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut x = record(
            "x",
            RecordState::Live,
            Some("window:4"),
            Some("codex resume xxx"),
        );
        x.position = 9;
        let mut gone = record("gone", RecordState::Closed, None, Some("claude --resume g"));
        gone.position = 10;
        {
            let mut st = s.lock().unwrap();
            st.upsert(x);
            st.upsert(gone);
        }
        for (id, why) in [
            ("b", NO_RESUME),
            ("x", crate::rex::agents::CANT_FORK),
            ("gone", super::NO_AGENT),
            ("nope", super::NO_AGENT),
        ] {
            let got = hub.agent_op(&s, &open(None), &AgentOp::Fork(id.into()), 7);
            assert_eq!(got, Err(why.to_string()), "{id}");
        }
        assert!(f.requests_for("session.new_window").is_empty());
        assert!(f.requests_for("session.create").is_empty());
    }

    #[test]
    fn a_new_agent_in_a_lost_session_recreates_it_typing_cmd() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        )]);

        let got = hub
            .agent_op(&s, &open(None), &AgentOp::New, 7)
            .expect("new");
        assert_eq!(f.requests_for("session.create").len(), 1);
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
        assert_eq!(got.agents.len(), 1);
        assert_eq!(got.agents[0].1.block_id, "block:new");
        assert_ne!(got.selected.as_deref(), Some("a"));
    }

    #[test]
    fn agent_list_shows_status_groups_and_the_closed_fold() {
        use crate::rex::windows::{AgentState, BlockStatus};
        let mut a = record("a", RecordState::Live, Some("window:1"), None);
        a.selected = true;
        let mut b = record(
            "b",
            RecordState::Live,
            Some("window:2"),
            Some("codex resume b"),
        );
        b.group = Some("g".into());
        let mut c = record("c", RecordState::Asleep, None, Some("claude --resume c"));
        c.group = Some("g".into());
        let d = record("d", RecordState::Live, Some("window:4"), None);
        let mut e = record("e", RecordState::Closed, None, Some("claude --resume e"));
        e.closed_at = Some(3);
        let mut f = record("f", RecordState::Closed, None, Some("claude --resume f"));
        f.closed_at = Some(5);
        let s = store(vec![a, b, c, d, e, f]);
        s.lock().unwrap().set_folded("t", "g", true);
        let status = |block: &str| match block {
            "block:1" => Some(BlockStatus {
                state: AgentState::Blocked,
                kind: None,
                msg: Some("needs you".into()),
                title: Some("t1".into()),
                asked: Some("fix it".into()),
                answered: Some("b".into()),
                said: Some("Fixed.".into()),
                topic: Some("Parser quoting".into()),
                needs: Some("Pick a or b".into()),
                background: None,
                cache: None,
            }),
            "block:2" => Some(BlockStatus {
                state: AgentState::Done,
                kind: None,
                msg: None,
                title: Some("finished".into()),
                asked: None,
                answered: None,
                said: None,
                topic: None,
                needs: None,
                background: None,
                cache: None,
            }),
            _ => None,
        };
        let list = agent_list("t", &s.lock().unwrap(), &Attention::default(), status);
        assert_eq!(list.tab_id, "t");
        let rows: Vec<(&str, &str, Option<&str>)> = list
            .rows
            .iter()
            .map(|r| (r.id.as_str(), r.state, r.context.as_deref()))
            .collect();
        assert_eq!(
            rows,
            [
                ("a", "blocked", Some("needs you")),
                ("d", "idle", None),
                ("b", "done", Some("finished")),
                ("c", "asleep", None),
            ],
            "ungrouped above the groups"
        );
        let summary: Vec<[Option<&str>; 6]> = list
            .rows
            .iter()
            .map(|r| {
                [
                    r.asked.as_deref(),
                    r.answered.as_deref(),
                    r.doing.as_deref(),
                    r.said.as_deref(),
                    r.topic.as_deref(),
                    r.needs.as_deref(),
                ]
            })
            .collect();
        assert_eq!(
            summary,
            [
                [
                    Some("fix it"),
                    Some("b"),
                    Some("needs you"),
                    Some("Fixed."),
                    Some("Parser quoting"),
                    Some("Pick a or b"),
                ],
                [None; 6],
                [None; 6],
                [None; 6],
            ],
            "a live row's own words; an asleep one has none"
        );
        assert!(list.rows[0].selected);
        assert!(!list.rows[0].can_sleep);
        assert_eq!(list.rows[0].sleep_reason.as_deref(), Some(NO_RESUME));
        assert!(list.rows[2].can_sleep && list.rows[2].sleep_reason.is_none());
        assert!(!list.rows[3].can_sleep, "already asleep");
        let fork: Vec<(bool, Option<&str>)> = list
            .rows
            .iter()
            .map(|r| (r.can_fork, r.fork_reason.as_deref()))
            .collect();
        assert_eq!(
            fork,
            [
                (false, Some(NO_RESUME)),
                (false, Some(NO_RESUME)),
                (false, Some(crate::rex::agents::CANT_FORK)),
                (true, None),
            ],
            "an asleep agent forks too"
        );
        assert_eq!(list.groups.len(), 1);
        assert_eq!(list.groups[0].name, "g");
        assert!(list.groups[0].folded && !list.groups[0].amber);
        let closed: Vec<&str> = list.closed.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(closed, ["f", "e"], "most recent first");

        let v = serde_json::to_value(&list).unwrap();
        assert_eq!(v["tabId"], "t");
        assert_eq!(v["rows"][0]["canSleep"], false);
        assert_eq!(v["rows"][0]["sleepReason"], NO_RESUME);
        assert_eq!(v["closed"][0]["closedAt"], 5);
        assert_eq!(v["groups"][0]["amber"], false);
    }

    #[test]
    fn needs_background_and_cache_hidden_while_working() {
        use crate::rex::windows::{AgentState, BlockStatus};
        let s = store(vec![record("a", RecordState::Live, Some("window:1"), None)]);
        let needs = |state: AgentState| {
            let status = |_: &str| {
                Some(BlockStatus {
                    state,
                    kind: None,
                    msg: None,
                    title: None,
                    asked: None,
                    answered: None,
                    said: None,
                    topic: None,
                    needs: Some("Pick a or b".into()),
                    background: Some("2 shells".into()),
                    cache: Some("1794445729".into()),
                })
            };
            let row =
                agent_list("t", &s.lock().unwrap(), &Attention::default(), status).rows[0].clone();
            (row.needs, row.background, row.cache)
        };
        let shown = (
            Some("Pick a or b".into()),
            Some("2 shells".into()),
            Some(1794445729),
        );
        assert_eq!(needs(AgentState::Done), shown);
        assert_eq!(needs(AgentState::Idle), shown);
        assert_eq!(
            needs(AgentState::Working),
            (None, None, None),
            "the last turn's question, background and cache"
        );
    }

    #[test]
    fn blocked_row_names_what_it_waits_on() {
        use crate::rex::windows::{AgentState, BlockStatus, BlockedKind};
        let s = store(vec![record("a", RecordState::Live, Some("window:1"), None)]);
        let row = |kind: Option<BlockedKind>| {
            let status = |_: &str| {
                Some(BlockStatus {
                    state: AgentState::Blocked,
                    kind,
                    msg: None,
                    title: None,
                    asked: None,
                    answered: None,
                    said: None,
                    topic: None,
                    needs: None,
                    background: None,
                    cache: None,
                })
            };
            agent_list("t", &s.lock().unwrap(), &Attention::default(), status).rows[0].blocked_kind
        };
        assert_eq!(row(Some(BlockedKind::Permission)), Some("permission"));
        assert_eq!(row(Some(BlockedKind::Question)), Some("question"));
        assert_eq!(row(Some(BlockedKind::Auth)), Some("auth"));
        assert_eq!(row(None), None);
    }

    #[test]
    fn done_unseen_marks_group_amber() {
        use crate::rex::windows::{AgentState, BlockStatus};
        let mut a = record("a", RecordState::Live, Some("window:1"), None);
        a.group = Some("g".into());
        let mut b = record("b", RecordState::Live, Some("window:2"), None);
        b.group = Some("g".into());
        let mut c = record("c", RecordState::Live, Some("window:3"), None);
        c.group = Some("h".into());
        let s = store(vec![a, b, c]);
        let state = std::cell::Cell::new(AgentState::Done);
        let status = |block: &str| {
            (block != "block:1").then(|| BlockStatus {
                state: state.get(),
                kind: None,
                msg: None,
                title: None,
                asked: None,
                answered: None,
                said: None,
                topic: None,
                needs: None,
                background: None,
                cache: None,
            })
        };
        let mut att = Attention::default();
        att.observe("block:2", AgentState::Working, false);
        att.observe("block:3", AgentState::Working, false);
        att.observe("block:2", AgentState::Done, false);
        att.observe("block:3", AgentState::Done, true);
        let amber = |att: &Attention| -> Vec<(String, bool)> {
            agent_list("t", &s.lock().unwrap(), att, status)
                .groups
                .into_iter()
                .map(|g| (g.name, g.amber))
                .collect()
        };
        assert_eq!(
            amber(&att),
            [("g".into(), true), ("h".into(), false)],
            "b finished unseen; c finished on screen"
        );
        att.seen("block:2");
        assert_eq!(amber(&att), [("g".into(), false), ("h".into(), false)]);
        state.set(AgentState::Blocked);
        assert_eq!(
            amber(&att),
            [("g".into(), true), ("h".into(), true)],
            "blocked needs you, seen or not"
        );
    }

    #[test]
    fn done_unseen_rows_are_unread() {
        use crate::rex::windows::{AgentState, BlockStatus};
        let b = record("b", RecordState::Live, Some("window:2"), None);
        let c = record("c", RecordState::Live, Some("window:3"), None);
        let d = record("d", RecordState::Asleep, None, None);
        let s = store(vec![b, c, d]);
        let state = std::cell::Cell::new(AgentState::Done);
        let status = |_: &str| {
            Some(BlockStatus {
                state: state.get(),
                kind: None,
                msg: None,
                title: None,
                asked: None,
                answered: None,
                said: None,
                topic: None,
                needs: None,
                background: None,
                cache: None,
            })
        };
        let mut att = Attention::default();
        att.observe("block:2", AgentState::Working, false);
        att.observe("block:3", AgentState::Working, false);
        att.observe("block:2", AgentState::Done, false);
        att.observe("block:3", AgentState::Done, true);
        let unread = |att: &Attention| -> Vec<(String, bool)> {
            agent_list("t", &s.lock().unwrap(), att, status)
                .rows
                .into_iter()
                .map(|r| (r.id, r.unread))
                .collect()
        };
        assert_eq!(
            unread(&att),
            [("b".into(), true), ("c".into(), false), ("d".into(), false)],
            "b finished hidden; c on screen; d asleep"
        );
        att.seen("block:2");
        assert!(unread(&att).iter().all(|(_, u)| !u), "seen clears it");

        att.observe("block:2", AgentState::Working, false);
        att.observe("block:2", AgentState::Done, false);
        assert_eq!(
            unread(&att)[0],
            ("b".into(), true),
            "done again: unread again"
        );
        state.set(AgentState::Working);
        att.observe("block:2", AgentState::Working, false);
        assert!(
            unread(&att).iter().all(|(_, u)| !u),
            "back to working clears it"
        );
        state.set(AgentState::Error);
        att.observe("block:2", AgentState::Error, false);
        assert_eq!(unread(&att)[0], ("b".into(), true), "error reads as done");

        state.set(AgentState::Blocked);
        att.observe("block:2", AgentState::Working, false);
        att.observe("block:2", AgentState::Blocked, false);
        assert!(
            unread(&att).iter().all(|(_, u)| !u),
            "blocked keeps its own dot"
        );
        state.set(AgentState::Done);
        att.observe("block:2", AgentState::Done, false);
        assert_eq!(
            unread(&att)[0],
            ("b".into(), true),
            "blocked then done: unread"
        );
    }

    /// A fake listing `session:old` (tab `t`'s main pane, windows `window:1` and `window:2`),
    /// with `split`, `session:split` mapped to `t`'s split, and `extra` unowned sessions,
    /// with `t`'s main pane attached, so the hub follows its windows.
    fn server_with(
        split: bool,
        extra: &[(&'static str, &'static str)],
        records: Vec<crate::rex::agents::AgentRecord>,
    ) -> (
        rex_client::fake::FakeServer,
        tempfile::TempDir,
        crate::rex::RexHub,
        std::sync::Mutex<crate::rex::agents::AgentStore>,
    ) {
        let f = fake(
            Some("session:old"),
            vec![("window:1", "block:1", "a"), ("window:2", "block:2", "b")],
        );
        let mut listed = vec![json!({"session_id": "session:old", "label": "t · main"})];
        if split {
            listed.push(json!({"session_id": "session:split", "label": "t · split"}));
        }
        for (id, label) in extra {
            listed.push(json!({"session_id": id, "label": label}));
        }
        f.reply("session.attach_all", json!({ "sessions": listed }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rex-sessions.json");
        map(&path, "session:old");
        if split {
            crate::rex::store::RexStore::load(path).set(
                &crate::registry::secondary_id("t"),
                Some(crate::rex::store::PaneRecord {
                    server: "local".into(),
                    session_id: "session:split".into(),
                    instance: None,
                }),
            );
        }
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(records);
        hub.agent_op(&s, &open(None), &AgentOp::Refresh, 7)
            .expect("attach");
        (f, dir, hub, s)
    }

    fn title(key: &str) -> Option<String> {
        (key == "t").then(|| "Proj".to_owned())
    }

    #[test]
    fn plan_lists_terminal_shells_unresumable_agents_and_unowned_sessions_as_losses() {
        use super::{LossItem, LossKind};
        let (_f, _dir, hub, s) = server_with(
            true,
            &[("session:phone", "phone")],
            vec![
                record(
                    "a",
                    RecordState::Live,
                    Some("window:1"),
                    Some("claude --resume aaa"),
                ),
                record("b", RecordState::Live, Some("window:2"), None),
                record("c", RecordState::Asleep, None, Some("claude --resume ccc")),
                record("d", RecordState::Closed, None, Some("claude --resume ddd")),
            ],
        );

        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert_eq!(plan.sleep, ["a"], "only a live, resumable agent sleeps");
        let item = |kind, name: &str| LossItem {
            kind,
            name: name.into(),
        };
        assert_eq!(
            plan.lose,
            [
                item(LossKind::Agent, "Proj · b"),
                item(LossKind::Session, "phone"),
            ],
            "the scratch column ends unlisted"
        );
        let v = serde_json::to_value(&plan.lose[1]).unwrap();
        assert_eq!(v, json!({"kind": "session", "name": "phone"}));
    }

    #[test]
    fn plan_reads_the_windows_of_a_session_the_hub_doesnt_follow() {
        use super::{LossItem, LossKind};
        let f = fake(Some("session:old"), vec![]);
        let read = crate::rex::agent_open::tests::view(
            "session:old",
            &[
                ("window:1", "block:1", "a"),
                ("window:9", "block:9", "stray"),
            ],
        );
        f.reply("session.view", read["view"].clone());
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.connect("local").expect("connect");
        let s = store(vec![record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        )]);

        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert_eq!(f.requests_for("session.view").len(), 1);
        assert_eq!(plan.sleep, ["a"]);
        assert_eq!(
            plan.lose,
            [LossItem {
                kind: LossKind::Agent,
                name: "Proj · stray".into(),
            }],
            "a window no record knows still ends"
        );
        assert_eq!(
            state_of(&s, "a").unwrap().state,
            RecordState::Live,
            "planning changes no record"
        );
    }

    #[test]
    fn nothing_unrecoverable_skips_confirm() {
        let (_f, _dir, hub, s) = server_with(
            false,
            &[],
            vec![
                record(
                    "a",
                    RecordState::Live,
                    Some("window:1"),
                    Some("claude --resume aaa"),
                ),
                record(
                    "b",
                    RecordState::Live,
                    Some("window:2"),
                    Some("codex resume bbb"),
                ),
            ],
        );
        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert_eq!(plan.sleep, ["a", "b"]);
        assert!(plan.lose.is_empty());
        assert!(
            plan.unconfirmed(&[]).is_empty(),
            "goes ahead without asking"
        );

        let (_f, _dir, hub, s) = server_with(
            true,
            &[("session:phone", "phone")],
            vec![record(
                "a",
                RecordState::Live,
                Some("window:1"),
                Some("claude --resume aaa"),
            )],
        );
        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        let named: Vec<String> = plan.lose.iter().map(|l| l.name.clone()).collect();
        assert!(plan.unconfirmed(&named).is_empty(), "every loss confirmed");
        let fewer = &named[..named.len() - 1];
        assert_eq!(
            plan.unconfirmed(fewer).len(),
            1,
            "a loss not confirmed asks again"
        );
    }

    #[test]
    fn sleep_server_records_then_stops() {
        use std::sync::{Arc, Mutex};
        let (f, _dir, hub, s) = server_with(
            false,
            &[],
            vec![
                record(
                    "a",
                    RecordState::Live,
                    Some("window:1"),
                    Some("claude --resume aaa"),
                ),
                record(
                    "b",
                    RecordState::Live,
                    Some("window:2"),
                    Some("codex resume bbb"),
                ),
            ],
        );
        let s = Arc::new(s);
        let at_stop: Arc<Mutex<Option<Vec<RecordState>>>> = Arc::default();
        let (seen, store) = (at_stop.clone(), s.clone());
        f.on("server.stop", move |_, _| {
            let states = store
                .lock()
                .unwrap()
                .records("t")
                .into_iter()
                .map(|r| r.state)
                .collect();
            *seen.lock().unwrap() = Some(states);
            rex_client::fake::Reply::ok(json!({}))
        });

        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert_eq!(
            hub.sleep_server(&s, "local", &plan, &title)
                .expect("sleep server"),
            super::Slept::Done
        );

        assert_eq!(
            *at_stop.lock().unwrap(),
            Some(vec![RecordState::Asleep, RecordState::Asleep]),
            "asleep before the stop"
        );
        let methods: Vec<String> = f
            .requests()
            .into_iter()
            .map(|r| r.method)
            .filter(|m| m == "session.close_window" || m == "server.stop")
            .collect();
        assert_eq!(
            methods,
            [
                "session.close_window",
                "session.close_window",
                "server.stop"
            ]
        );
        let a = state_of(&s, "a").unwrap();
        assert_eq!((a.window_id, a.block_id), (None, None));
        assert!(
            hub.live_sessions("local").is_empty(),
            "its sessions are gone"
        );
        assert!(!hub.session_alive("t"));
    }

    #[test]
    fn a_stopped_server_reads_as_gone_until_a_tab_opens_it() {
        let (f, _dir, hub, s) = server_with(
            true,
            &[("session:phone", "phone")],
            vec![record(
                "a",
                RecordState::Live,
                Some("window:1"),
                Some("claude --resume aaa"),
            )],
        );
        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert_eq!(
            hub.sleep_server(&s, "local", &plan, &title)
                .expect("sleep server"),
            super::Slept::Done
        );
        assert!(f.stopped());
        assert_eq!(f.wait_control_connections(0, Duration::from_secs(5)), 0);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(hub.servers(), ["local"], "heard, with nothing on it");
        assert!(hub.live_sessions("local").is_empty());
        assert_eq!(hub.stoppable("local"), Err(super::NOT_RUNNING));
        assert!(hub.agent_panes("local", "session:old").is_empty());
    }

    #[test]
    fn a_window_opened_after_the_plan_asks_again_instead_of_stopping() {
        use super::{LossItem, LossKind, Slept};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let f = fake(Some("session:old"), vec![]);
        let reads = AtomicUsize::new(0);
        f.on("session.view", move |_, _| {
            let windows: &[(&str, &str, &str)] = match reads.fetch_add(1, Ordering::SeqCst) {
                0 => &[("window:1", "block:1", "a")],
                _ => &[("window:9", "block:9", "stray")],
            };
            let read = crate::rex::agent_open::tests::view("session:old", windows);
            rex_client::fake::Reply::ok(read["view"].clone())
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.connect("local").expect("connect");
        let s = store(vec![record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        )]);

        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert!(plan.lose.is_empty(), "nothing to confirm at first");
        let got = hub
            .sleep_server(&s, "local", &plan, &title)
            .expect("sleep server");

        assert_eq!(
            got,
            Slept::Unconfirmed(super::SleepPlan {
                sleep: vec![],
                lose: vec![LossItem {
                    kind: LossKind::Agent,
                    name: "Proj · stray".into(),
                }],
            })
        );
        assert!(f.requests_for("server.stop").is_empty(), "nothing stops");
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
        assert_eq!(hub.stoppable("local"), Ok(()));
    }

    #[test]
    fn a_refused_stop_leaves_the_agents_asleep_and_the_server_listed() {
        let (f, _dir, hub, s) = server_with(
            false,
            &[],
            vec![record(
                "a",
                RecordState::Live,
                Some("window:1"),
                Some("claude --resume aaa"),
            )],
        );
        f.fail("server.stop", "forbidden", "local socket only", 403);
        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        assert!(hub.sleep_server(&s, "local", &plan, &title).is_err());
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
        assert_eq!(hub.live_sessions("local").len(), 1);
        assert_eq!(hub.stoppable("local"), Ok(()));
    }

    /// Tab `t`'s two pane keys: its main pane and its split.
    fn t_panes() -> Vec<String> {
        vec!["t".into(), crate::registry::secondary_id("t")]
    }

    #[test]
    fn plan_project_lists_its_unresumable_agents_only() {
        use super::{LossItem, LossKind};
        let (_f, _dir, hub, s) = server_with(
            true,
            &[("session:phone", "phone")],
            vec![
                record(
                    "a",
                    RecordState::Live,
                    Some("window:1"),
                    Some("claude --resume aaa"),
                ),
                record("b", RecordState::Live, Some("window:2"), None),
            ],
        );
        let plan = hub
            .plan_project_sleep(&s, "local", &t_panes(), &title)
            .expect("plan");
        assert_eq!(plan.sleep, ["a"]);
        let item = |kind, name: &str| LossItem {
            kind,
            name: name.into(),
        };
        assert_eq!(
            plan.lose,
            [item(LossKind::Agent, "Proj · b")],
            "its scratch column and another project's session are no loss"
        );
    }

    #[test]
    fn sleep_project_sleeps_then_destroys() {
        use std::sync::{Arc, Mutex};
        let (f, _dir, hub, s) = server_with(
            false,
            &[],
            vec![
                record(
                    "a",
                    RecordState::Live,
                    Some("window:1"),
                    Some("claude --resume aaa"),
                ),
                record(
                    "b",
                    RecordState::Live,
                    Some("window:2"),
                    Some("codex resume bbb"),
                ),
            ],
        );
        let s = Arc::new(s);
        let at_destroy: Arc<Mutex<Option<Vec<RecordState>>>> = Arc::default();
        let (seen, store) = (at_destroy.clone(), s.clone());
        f.on("session.destroy", move |_, _| {
            let states = store
                .lock()
                .unwrap()
                .records("t")
                .into_iter()
                .map(|r| r.state)
                .collect();
            *seen.lock().unwrap() = Some(states);
            rex_client::fake::Reply::ok(json!({}))
        });

        let plan = hub
            .plan_project_sleep(&s, "local", &t_panes(), &title)
            .expect("plan");
        assert_eq!(
            hub.sleep_project(&s, "local", &t_panes(), &plan, &title)
                .expect("sleep project"),
            super::Slept::Done
        );
        assert_eq!(
            *at_destroy.lock().unwrap(),
            Some(vec![RecordState::Asleep, RecordState::Asleep]),
            "asleep before the session ends"
        );
        assert_eq!(f.requests_for("session.destroy").len(), 1);
        assert!(f.requests_for("server.stop").is_empty());
        let states: Vec<RecordState> = s
            .lock()
            .unwrap()
            .records("t")
            .into_iter()
            .map(|r| r.state)
            .collect();
        assert_eq!(states, [RecordState::Asleep, RecordState::Asleep]);
        assert!(!hub.session_alive("t"));
    }

    #[test]
    fn sleep_project_new_loss_asks_again() {
        use super::{LossItem, LossKind, SleepPlan, Slept};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let f = fake(Some("session:old"), vec![]);
        let reads = AtomicUsize::new(0);
        f.on("session.view", move |_, _| {
            let windows: &[(&str, &str, &str)] = match reads.fetch_add(1, Ordering::SeqCst) {
                0 => &[("window:1", "block:1", "a")],
                _ => &[("window:9", "block:9", "stray")],
            };
            let read = crate::rex::agent_open::tests::view("session:old", windows);
            rex_client::fake::Reply::ok(read["view"].clone())
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.connect("local").expect("connect");
        let s = store(vec![record(
            "a",
            RecordState::Live,
            Some("window:1"),
            Some("claude --resume aaa"),
        )]);

        let plan = hub
            .plan_project_sleep(&s, "local", &t_panes(), &title)
            .expect("plan");
        assert!(plan.lose.is_empty());
        let got = hub
            .sleep_project(&s, "local", &t_panes(), &plan, &title)
            .expect("sleep project");
        assert_eq!(
            got,
            Slept::Unconfirmed(SleepPlan {
                sleep: vec![],
                lose: vec![LossItem {
                    kind: LossKind::Agent,
                    name: "Proj · stray".into(),
                }],
            })
        );
        assert!(f.requests_for("session.destroy").is_empty(), "nothing ends");
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
    }

    #[test]
    fn sleep_project_with_no_session_only_marks_asleep() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.connect("local").expect("connect");
        let s = store(vec![record(
            "a",
            RecordState::Asleep,
            None,
            Some("claude --resume aaa"),
        )]);
        let plan = hub
            .plan_project_sleep(&s, "local", &t_panes(), &title)
            .expect("plan");
        assert_eq!(plan, super::SleepPlan::default());
        assert_eq!(
            hub.sleep_project(&s, "local", &t_panes(), &plan, &title)
                .expect("sleep project"),
            super::Slept::Done
        );
        assert!(f.requests_for("session.destroy").is_empty());
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
    }

    #[test]
    fn sleep_project_ends_a_restored_session_rather_than_leaving_it_unclaimed() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        f.reply("server.status", json!({"instance_id": "b"}));
        let dir = tempfile::tempdir().unwrap();
        crate::rex::store::RexStore::load(dir.path().join("rex-sessions.json")).set(
            "t",
            Some(crate::rex::store::PaneRecord {
                server: "local".into(),
                session_id: "session:old".into(),
                instance: Some("a".into()),
            }),
        );
        let (hub, _) = hub(f.endpoint(), &dir);
        hub.connect("local").expect("connect");
        let s = store(vec![]);
        let plan = hub
            .plan_project_sleep(&s, "local", &t_panes(), &title)
            .expect("plan");
        hub.sleep_project(&s, "local", &t_panes(), &plan, &title)
            .expect("sleep project");
        let destroyed = f.requests_for("session.destroy");
        assert_eq!(destroyed.len(), 1, "its bare shells end with the project");
        assert_eq!(destroyed[0].payload["session_id"], "session:old");
    }

    #[test]
    fn a_server_warden_stopped_starting_again_is_no_restart() {
        let f = fake(Some("session:old"), vec![]);
        let instance = std::sync::Arc::new(std::sync::Mutex::new("a"));
        let held = instance.clone();
        f.on("server.status", move |_, _| {
            rex_client::fake::Reply::ok(json!({"instance_id": *held.lock().unwrap()}))
        });
        // Replies without stopping, so the same fake can stand in for the next start.
        f.on("server.stop", |_, _| rex_client::fake::Reply::ok(json!({})));
        let dir = tempfile::tempdir().unwrap();
        let (hub, _) = hub(f.endpoint(), &dir);
        let rx = hub.subscribe();
        hub.connect("local").expect("connect");
        let s = store(vec![]);
        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        hub.sleep_server(&s, "local", &plan, &title)
            .expect("sleep server");
        *instance.lock().unwrap() = "b";
        hub.connect("local").expect("start again");
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, crate::rex::HubEvent::Restarted { .. }),
                "warden stopped it: the opens that start it again adopt as they go"
            );
        }
    }

    #[test]
    fn sleep_project_on_a_stopped_server_ends_cleanly() {
        let (_f, _dir, hub, s) = server_with(
            true,
            &[],
            vec![record(
                "a",
                RecordState::Live,
                Some("window:1"),
                Some("claude --resume aaa"),
            )],
        );
        let plan = hub.plan_server_sleep(&s, "local", &title).expect("plan");
        hub.sleep_server(&s, "local", &plan, &title)
            .expect("sleep server");

        let plan = hub
            .plan_project_sleep(&s, "local", &t_panes(), &title)
            .expect("plan");
        assert_eq!(plan, super::SleepPlan::default());
        assert_eq!(
            hub.sleep_project(&s, "local", &t_panes(), &plan, &title),
            Ok(super::Slept::Done),
            "nothing listed, so nothing to reach the server for"
        );
    }

    /// A list of rows `(id, group)` in list order, `selected` selected, `folded` groups folded.
    fn list(rows: &[(&str, Option<&str>)], selected: &str, folded: &[&str]) -> super::AgentListDto {
        let mut groups: Vec<super::AgentGroupDto> = Vec::new();
        for (_, g) in rows {
            if let Some(g) = g {
                if !groups.iter().any(|x| x.name == *g) {
                    groups.push(super::AgentGroupDto {
                        name: (*g).into(),
                        folded: folded.contains(g),
                        amber: false,
                    });
                }
            }
        }
        super::AgentListDto {
            tab_id: "t".into(),
            rows: rows
                .iter()
                .map(|(id, g)| super::AgentRowDto {
                    id: (*id).into(),
                    name: (*id).into(),
                    group: g.map(Into::into),
                    parent: None,
                    depth: 0,
                    forks: 0,
                    folded: false,
                    hidden: false,
                    amber: false,
                    state: "idle",
                    blocked_kind: None,
                    context: None,
                    asked: None,
                    doing: None,
                    answered: None,
                    said: None,
                    topic: None,
                    needs: None,
                    background: None,
                    cache: None,
                    selected: *id == selected,
                    can_sleep: true,
                    sleep_reason: None,
                    can_fork: true,
                    fork_reason: None,
                    unread: false,
                })
                .collect(),
            groups,
            closed: vec![],
        }
    }

    const ROWS: &[(&str, Option<&str>)] = &[
        ("a", None),
        ("b", None),
        ("c", None),
        ("d", Some("g")),
        ("e", Some("g")),
    ];

    #[test]
    fn step_wraps_through_visible_rows() {
        use super::{agent_step, Step};
        assert_eq!(
            agent_step(&list(ROWS, "c", &[]), Step::Next).as_deref(),
            Some("d")
        );
        assert_eq!(
            agent_step(&list(ROWS, "e", &[]), Step::Next).as_deref(),
            Some("a")
        );
        assert_eq!(
            agent_step(&list(ROWS, "a", &[]), Step::Prev).as_deref(),
            Some("e")
        );
        assert_eq!(
            agent_step(&list(ROWS, "b", &[]), Step::Prev).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn step_skips_folded_groups() {
        use super::{agent_step, Step};
        assert_eq!(
            agent_step(&list(ROWS, "c", &["g"]), Step::Next).as_deref(),
            Some("a")
        );
        assert_eq!(
            agent_step(&list(ROWS, "a", &["g"]), Step::Prev).as_deref(),
            Some("c")
        );
    }

    #[test]
    fn step_needs_two_visible_rows() {
        use super::{agent_step, Step};
        let rows = &[("a", None), ("d", Some("g")), ("e", Some("g"))];
        assert_eq!(agent_step(&list(rows, "a", &["g"]), Step::Next), None);
        assert_eq!(agent_step(&list(rows, "a", &["g"]), Step::Prev), None);
        assert_eq!(
            agent_step(&list(ROWS, "nobody", &[]), Step::Next),
            None,
            "no selection"
        );
    }

    #[test]
    fn move_stays_in_group() {
        use super::{agent_move, Step};
        let at = |sel, step| agent_move(&list(ROWS, sel, &[]), step);
        assert_eq!(at("b", Step::Prev), Some(("b".into(), 0)));
        assert_eq!(at("b", Step::Next), Some(("b".into(), 2)));
        assert_eq!(at("a", Step::Prev), None, "top of its group");
        assert_eq!(at("c", Step::Next), None, "bottom of its group");
        assert_eq!(at("d", Step::Prev), None);
        assert_eq!(at("d", Step::Next), Some(("d".into(), 1)));
        assert_eq!(at("e", Step::Next), None);
    }

    /// `list` with `forks` as `(id, its parent)`, hiding those under a folded `folded_family`.
    fn family_list(selected: &str, forks: &[(&str, &str)], hidden: &[&str]) -> super::AgentListDto {
        let rows = &[("a", None), ("a1", None), ("a2", None), ("b", None)];
        let mut l = list(rows, selected, &[]);
        for r in &mut l.rows {
            r.parent = forks
                .iter()
                .find(|(id, _)| *id == r.id)
                .map(|(_, p)| (*p).into());
            r.hidden = hidden.contains(&r.id.as_str());
        }
        l
    }

    #[test]
    fn agent_move_steps_among_siblings_only() {
        use super::{agent_move, Step};
        let forks = &[("a1", "a"), ("a2", "a")];
        let at = |sel, step| agent_move(&family_list(sel, forks, &[]), step);
        assert_eq!(at("a1", Step::Prev), None, "first of a's forks");
        assert_eq!(at("a1", Step::Next), Some(("a1".into(), 1)));
        assert_eq!(at("a2", Step::Next), None, "last of a's forks");
        assert_eq!(
            at("a", Step::Next),
            Some(("a".into(), 1)),
            "past b, with its forks"
        );
    }

    #[test]
    fn agent_step_skips_rows_in_a_folded_family() {
        use super::{agent_step, Step};
        let l = family_list("a", &[("a1", "a"), ("a2", "a")], &["a1", "a2"]);
        assert_eq!(agent_step(&l, Step::Next).as_deref(), Some("b"));
    }

    #[test]
    fn agent_list_carries_parent_depth_and_fold() {
        use crate::rex::windows::{AgentState, BlockStatus};
        let mut a = record("a", RecordState::Live, Some("window:1"), None);
        a.folded = true;
        let mut a1 = record("a1", RecordState::Live, Some("window:2"), None);
        a1.lineage = vec!["a".into()];
        let mut a1x = record("a1x", RecordState::Live, Some("window:3"), None);
        a1x.lineage = vec!["a1".into(), "a".into()];
        let s = store(vec![a, a1, a1x]);
        let status = |block: &str| {
            Some(BlockStatus {
                state: if block == "block:2" {
                    AgentState::Blocked
                } else {
                    AgentState::Idle
                },
                kind: None,
                msg: None,
                title: None,
                asked: None,
                answered: None,
                said: None,
                topic: None,
                needs: None,
                background: None,
                cache: None,
            })
        };
        let l = agent_list("t", &s.lock().unwrap(), &Attention::default(), status);
        let got: Vec<_> = l
            .rows
            .iter()
            .map(|r| {
                (
                    r.id.as_str(),
                    r.parent.as_deref(),
                    r.depth,
                    r.forks,
                    r.folded,
                    r.hidden,
                    r.amber,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a", None, 0, 2, true, false, true),
                ("a1", Some("a"), 1, 1, false, true, false),
                ("a1x", Some("a1"), 2, 0, false, true, false),
            ]
        );
    }

    #[test]
    fn a_server_not_running_cant_sleep() {
        let (f, _dir, hub, _s) = server_with(false, &[], vec![]);
        assert_eq!(hub.stoppable("local"), Ok(()));
        assert_eq!(hub.stoppable("remote"), Err(super::NOT_RUNNING));
        drop(f);
        eventually("the connection to drop", || {
            hub.stoppable("local") == Err(super::NOT_RUNNING)
        });
    }

    #[test]
    fn rename_relabels_a_live_window_and_keeps_an_asleep_name() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![
            record("a", RecordState::Live, Some("window:1"), None),
            record("b", RecordState::Asleep, None, Some("claude --resume bbb")),
        ]);
        hub.agent_op(
            &s,
            &open(None),
            &AgentOp::Rename("a".into(), " review ".into()),
            7,
        )
        .expect("rename live");
        let set = f.requests_for("session.set_window_label");
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].payload["window_id"], "window:1");
        assert_eq!(state_of(&s, "a").unwrap().name, "review");

        hub.agent_op(
            &s,
            &open(None),
            &AgentOp::Rename("b".into(), "later".into()),
            7,
        )
        .expect("rename asleep");
        assert_eq!(f.requests_for("session.set_window_label").len(), 1);
        assert_eq!(state_of(&s, "b").unwrap().name, "later");
    }
}
