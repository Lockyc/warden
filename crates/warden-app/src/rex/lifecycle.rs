//! The agent tab list's lifecycle: new, wake, sleep, close, reopen and rename an agent tab, and
//! follow windows that change in Rex — each one pass over the pane's records under the pane's
//! hold ([`RexHub::agent_op`]) — plus what the list shows ([`agent_list`]).
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
use super::windows::{AgentState, BlockStatus};
use super::{budgeted, lock, HubEvent, RexHub, OPEN_BUDGET};
use crate::registry::{AgentsOpened, RexOpen};
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
    /// Name the tab; a live one's window is relabelled in Rex.
    Rename(String, String),
    /// Follow the session's windows as they are now.
    Refresh,
}

/// Why `op` can't run on `records`: sleeping, or closing unconfirmed, an agent that never
/// reported its resume command ([`NO_RESUME`]).
pub fn refusal(op: &AgentOp, records: &[AgentRecord]) -> Option<&'static str> {
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
        let deadline = Instant::now() + OPEN_BUDGET;
        let done = match self.inner.hold_pane(&req.pane_key, Some(deadline)) {
            Ok(_held) => self.agent_op_until(store, req, op, now, deadline),
            Err(e) => Err(Failed::Rex(e)),
        };
        match done {
            Ok(opened) => Ok(opened),
            Err(Failed::Refused(why)) => Err(why.to_owned()),
            Err(Failed::Rex(e)) => match budgeted::<()>(deadline, Err(e)) {
                Err(e) => Err(super::spawn::open_error(&req.server, &e)),
                Ok(()) => Err(NO_AGENT.to_owned()),
            },
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
    ) -> Result<AgentsOpened, Failed> {
        let (project, server) = (req.pane_key.as_str(), req.server.as_str());
        let held = lock(store).records(project);
        if let Some(why) = refusal(op, &held) {
            return Err(Failed::Refused(why));
        }
        let mut session = self.current_session(project, server, deadline)?;
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
        match op {
            AgentOp::Refresh => {}
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
                    self.close_window_of(server, session.as_ref(), &mut records[i])?;
                    records[i].state = RecordState::Asleep;
                }
            }
            AgentOp::Close { id, .. } => {
                let i = at(&records, id)?;
                self.close_window_of(server, session.as_ref(), &mut records[i])?;
                let r = &mut records[i];
                if r.resume.is_none() {
                    drop = Some(r.id.clone());
                }
                if r.state != RecordState::Closed {
                    r.state = RecordState::Closed;
                    r.closed_at = Some(now);
                }
            }
            AgentOp::Rename(id, name) => {
                let i = at(&records, id)?;
                let name = name.trim();
                let window = records[i]
                    .window_id
                    .clone()
                    .filter(|_| records[i].state == RecordState::Live);
                if let (Some((_, sid)), Some(w)) = (&session, window) {
                    self.set_window_label(server, sid, &w, name)?;
                }
                if !name.is_empty() {
                    records[i].name = name.to_owned();
                }
            }
        }
        renumber(&mut records);
        let shown = settle(&mut records, want);
        if let Some(id) = drop {
            records.retain(|r| r.id != id);
        }
        let shown = shown.and_then(|i| {
            let id = records.get(i).map(|r| r.id.clone())?;
            records.iter().position(|r| r.id == id)
        });
        let opened = self.bind_agents(project, session.as_ref(), &records, shown);
        lock(store).merge(&held, records);
        Ok(opened)
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
    ) -> Result<(), RexError> {
        if let (Some((_, sid)), Some(w)) = (session, record.window_id.as_deref()) {
            self.close_window(server, sid, w)?;
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
    /// A Rex tab's terminal pane: a plain shell, with nothing to resume.
    Shell,
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

/// Sleeping a server: the agent records that go asleep, and what is lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerSleepPlan {
    pub sleep: Vec<String>,
    pub lose: Vec<LossItem>,
}

impl ServerSleepPlan {
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
    /// is a shell lost; one nothing maps is a session lost. `title` names a tab by its key. Reads
    /// the network (worker only); `Err` when a session's windows can't be read, since the plan
    /// would then miss what it loses.
    pub fn plan_server_sleep(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ServerSleepPlan, RexError> {
        let (sessions, mapped) = {
            let st = self.inner.lock();
            let sessions = st.sessions(server).map(<[_]>::to_vec).unwrap_or_default();
            (sessions, st.store.panes_on(server))
        };
        let mut plan = ServerSleepPlan::default();
        let (mut agents, mut shells, mut unowned) = (Vec::new(), Vec::new(), Vec::new());
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
                if let Some(tab) = crate::registry::tab_of_secondary(key) {
                    shells.push(LossItem {
                        kind: LossKind::Shell,
                        name: title(tab).map_or_else(label, |t| format!("{t} · terminal")),
                    });
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
        plan.lose = [agents, shells, unowned].concat();
        Ok(plan)
    }

    /// Sleep `server` as `plan` says: each of its records, one at a time under its pane's hold,
    /// has its window closed and is stored asleep — so a sleep cut short anywhere leaves every
    /// record matching Rex, and a refused stop leaves the agents asleep rather than stranded —
    /// then `server.stop`. Once Rex has stopped, the hub forgets the server's sessions and doesn't
    /// reconnect: only a tab open starts it again.
    pub fn sleep_server(
        &self,
        store: &Mutex<AgentStore>,
        server: &str,
        plan: &ServerSleepPlan,
    ) -> Result<(), RexError> {
        for id in &plan.sleep {
            let Some(project) = lock(store).project_of_record(id).filter(|p| {
                self.inner
                    .lock()
                    .store
                    .get(p)
                    .is_some_and(|r| r.server == server)
            }) else {
                continue;
            };
            let _held = self
                .inner
                .hold_pane(&project, Some(Instant::now() + OPEN_BUDGET))?;
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
                self.close_window(server, &sid, w)?;
            }
            r.window_id = None;
            r.block_id = None;
            r.state = RecordState::Asleep;
            lock(store).merge(&before, after);
        }
        let (client, _) = self.inner.current(server).ok_or(RexError::Unreachable)?;
        self.inner.lock().stopping.insert(server.to_owned());
        match client.call("server.stop", serde_json::json!({})) {
            Ok(_) => {
                drop(client);
                self.inner.forget_server(server);
                Ok(())
            }
            Err(e) => {
                self.inner.lock().stopping.remove(server);
                Err(e)
            }
        }
    }
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

/// Settle which of `records` (one project's) is selected — at most one: `want`, else the
/// selected one while it is live, else its nearest live neighbour in list order, else the first
/// live one. With nothing live the choice stays for the next open, unless it closed.
fn settle(records: &mut [AgentRecord], want: Option<usize>) -> Option<usize> {
    let live = |r: &AgentRecord| r.state == RecordState::Live;
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by_key(|&i| records[i].position);
    let shown = want.or_else(|| match records.iter().position(|r| r.selected) {
        Some(c) if live(&records[c]) => Some(c),
        Some(c) => {
            let at = order.iter().position(|&i| i == c)?;
            let flags: Vec<bool> = order.iter().map(|&i| live(&records[i])).collect();
            shell_core::pick_live_neighbour(at, &flags).map(|k| order[k])
        }
        None => order.iter().copied().find(|&i| live(&records[i])),
    });
    for (j, r) in records.iter_mut().enumerate() {
        match shown {
            Some(i) => r.selected = j == i,
            None if r.state == RecordState::Closed => r.selected = false,
            None => {}
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
    /// `working`, `blocked`, `done`, `error`, `idle` or `asleep`.
    pub state: &'static str,
    /// The program's status message, else its terminal title.
    pub context: Option<String>,
    pub selected: bool,
    pub can_sleep: bool,
    /// Why a live tab can't sleep.
    pub sleep_reason: Option<String>,
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
        rows.push(AgentRowDto {
            id: r.id.clone(),
            name: r.name.clone(),
            group: r.group.clone(),
            state,
            context: st.and_then(|s| s.msg.or(s.title)),
            selected: r.selected,
            can_sleep: live && r.resume.is_some(),
            sleep_reason: (live && r.resume.is_none()).then(|| NO_RESUME.to_owned()),
        });
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
/// (`warden:server-sleep-confirm`, every loss by name) and answers with this again. Otherwise
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
                let _ = main.emit_to(
                    label.as_str(),
                    "warden:server-sleep-confirm",
                    serde_json::json!({ "label": label, "server": server, "losses": plan.lose }),
                );
            }
            Ok(plan) => {
                let projects = let_go(&main, &server);
                stop_server(&main, label, server, plan, projects);
            }
        });
    })
}

/// Run `f` on a named worker thread; `Err` says it couldn't start.
fn worker(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(|_| ())
        .map_err(|e| format!("couldn't start {name}: {e}"))
}

/// Show `message` in window `label`'s error banner (main thread).
fn fail(app: &AppHandle, label: &str, message: &str) {
    eprintln!("warden: {label}: {message}");
    let _ = app.emit_to(
        label,
        "warden:error",
        serde_json::json!({ "label": label, "message": message }),
    );
}

/// Every pane on `server` lets go of its surface (main thread), so no relay sees its stream die
/// and reads it as an exit: a popped-out tab on it ends whole, as when its program exits
/// ([`WindowManager::end_detached`](crate::manager::WindowManager::end_detached)), and every
/// docked tab on it goes cold, as an unload. Returns the tabs, for their lists to repaint.
fn let_go(app: &AppHandle, server: &str) -> Vec<String> {
    let st = app.state::<ManagerState>();
    let mut projects = Vec::new();
    let mut exited = Vec::new();
    let mut ended = Vec::new();
    {
        let mut m = st.lock();
        let popped: Vec<String> = m
            .detached
            .iter()
            .filter(|(_, d)| d.spec.server.as_deref() == Some(server))
            .map(|(l, _)| l.clone())
            .collect();
        for dlabel in popped {
            ended.extend(m.end_detached(app, &dlabel));
        }
        let labels: Vec<String> = m.windows.keys().cloned().collect();
        for l in labels {
            let Some(ws) = m.windows.get_mut(&l) else {
                continue;
            };
            for (id, _) in ws
                .registry
                .rex_tabs()
                .into_iter()
                .filter(|(_, s)| s == server)
            {
                let new_active = ws.registry.unload(&id);
                exited.push((l.clone(), id.clone(), new_active));
                projects.push(id);
            }
            m.record_session(&l);
        }
    }
    for e in ended {
        crate::manager::announce_detached_ended(app, e);
    }
    for (l, id, new_active) in exited {
        let _ = app.emit_to(
            l.as_str(),
            "warden:tab-exited",
            serde_json::json!({ "label": l, "id": id, "newActive": new_active }),
        );
    }
    crate::sync_agent_menu(app);
    projects.sort();
    projects.dedup();
    projects
}

/// Sleep `server` as `plan` says on a worker, then repaint `projects`' lists, the server's
/// presence and the Sleep Server menu (main thread); a failure goes to window `label`'s banner.
fn stop_server(
    app: &AppHandle,
    label: String,
    server: String,
    plan: ServerSleepPlan,
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
                (Some(hub), Some(store)) => hub.sleep_server(&store, &server, &plan),
                _ => Err(RexError::Unreachable),
            };
            let main = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Err(e) = result {
                    fail(
                        &main,
                        &label,
                        &format!("couldn't sleep Rex server {server}: {e}"),
                    );
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
        if let AgentOp::Sleep(id) | AgentOp::Close { id, .. } = &op {
            ws.registry.release_agent(tab_id, id);
        }
        req
    };
    let (app, label) = (app.clone(), label.to_owned());
    std::thread::Builder::new()
        .name("warden-agent-op".into())
        .spawn(move || {
            let result = match (
                app.try_state::<RexHub>(),
                app.try_state::<Mutex<AgentStore>>(),
            ) {
                (Some(hub), Some(store)) => hub.agent_op(&store, &req, &op, unix_now()),
                _ => Err(format!("Rex server {}: no connection to Rex", req.server)),
            };
            let main = app.clone();
            let _ = app.run_on_main_thread(move || {
                land(&main, Some(&label), &req.pane_key, result);
            });
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
    m.record_session(label);
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
/// it and repaints; a failure goes to window `label`'s error banner. Then the list.
fn land(app: &AppHandle, label: Option<&str>, tab_id: &str, result: Result<AgentsOpened, String>) {
    let st = app.state::<ManagerState>();
    let mut m = st.lock();
    let mut refreshed = Vec::new();
    if let Ok(found) = &result {
        for (l, ws) in m.windows.iter_mut() {
            if ws.registry.spec_of(tab_id).is_none() {
                continue;
            }
            if let Err(e) = ws.registry.sync_agents(tab_id, found.clone()) {
                eprintln!("warden: couldn't open an agent of tab {tab_id:?}: {e}");
            }
            refreshed.push(l.clone());
        }
    }
    let dtos: Vec<_> = refreshed
        .iter()
        .filter_map(|l| {
            m.record_session(l);
            Some((l.clone(), m.init_dto(l)?))
        })
        .collect();
    drop(m);
    for (l, dto) in dtos {
        let _ = app.emit_to(l.as_str(), "warden:refresh", dto);
    }
    if let (Err(message), Some(label)) = (&result, label) {
        eprintln!("warden: {label}: {tab_id}: {message}");
        let _ = app.emit_to(
            label,
            "warden:error",
            serde_json::json!({ "label": label, "message": message }),
        );
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
        eprintln!("warden: agent tabs won't follow Rex: {e}");
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
            let body = status.and_then(|s| s.msg).unwrap_or_else(|| why.to_owned());
            crate::notify::raise(
                app,
                &label,
                &record.project,
                false,
                Some((format!("{title} · {}", record.name), body)),
                Some(&record.id),
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

/// Reconcile `project`'s list with its session's windows on a worker, if a window holds the tab.
fn refresh(app: &AppHandle, project: String) {
    let req = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows
            .values()
            .find_map(|ws| ws.registry.agent_request(&project))
    };
    let Some(req) = req else { return };
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("warden-agent-refresh".into())
        .spawn(move || {
            let (Some(hub), Some(store)) = (
                app.try_state::<RexHub>(),
                app.try_state::<Mutex<AgentStore>>(),
            ) else {
                return;
            };
            let result = hub.agent_op(&store, &req, &AgentOp::Refresh, unix_now());
            if let Err(e) = &result {
                eprintln!("warden: couldn't follow {project:?}'s agent tabs: {e}");
                return;
            }
            let main = app.clone();
            let _ = app.run_on_main_thread(move || land(&main, None, &project, result));
        });
    if let Err(e) = spawned {
        eprintln!("warden: couldn't start following agent tabs: {e}");
    }
}

/// Listen for the resume commands relays report ([`super::reports`]) for the app's life, storing
/// each on its record ([`AgentStore::report_resume`]) and re-sending that project's list. A
/// socket that can't be bound (a path past macOS's `sun_path`) leaves reports dropped; the app
/// runs on.
pub fn watch_reports(app: AppHandle) {
    let Some(path) = super::reports::socket_path(&app) else {
        eprintln!("warden: no app config dir, so agents can't report how to resume");
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let reports = match super::reports::listen(&path) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!(
                "warden: agents can't report how to resume ({}): {e}",
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
        eprintln!("warden: agents can't report how to resume: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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

        let got = hub.agent_op(&s, &open(false, None), &AgentOp::Sleep("a".into()), 7);
        assert_eq!(got, Err(NO_RESUME.to_string()));
        assert_eq!(NO_RESUME, "this agent hasn't reported how to resume it");
        assert!(f.requests_for("session.close_window").is_empty());
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Live);
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
            .agent_op(&s, &open(false, None), &AgentOp::Sleep("a".into()), 7)
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
                &open(false, Some("claude-work")),
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
            &open(false, None),
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

        hub.agent_op(&s, &open(false, None), &AgentOp::Reopen(None), 9)
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

        let got = hub.agent_op(&s, &open(false, None), &close(false), 7);
        assert_eq!(got, Err(NO_RESUME.to_string()));
        assert!(f.requests_for("session.close_window").is_empty());

        hub.agent_op(&s, &open(false, None), &close(true), 7)
            .expect("confirmed");
        assert_eq!(f.requests_for("session.close_window").len(), 1);
        assert_eq!(state_of(&s, "a"), None, "gone for good");
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
        hub.agent_op(&s, &open(true, None), &AgentOp::Refresh, 7)
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
            .agent_op(&s, &open(true, None), &AgentOp::Refresh, 8)
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
            .agent_op(&s, &open(false, None), &AgentOp::New, 7)
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
            .agent_op(&s, &open(false, None), &AgentOp::New, 7)
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
                msg: Some("needs you".into()),
                title: Some("t1".into()),
            }),
            "block:2" => Some(BlockStatus {
                state: AgentState::Done,
                msg: None,
                title: Some("finished".into()),
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
        assert!(list.rows[0].selected);
        assert!(!list.rows[0].can_sleep);
        assert_eq!(list.rows[0].sleep_reason.as_deref(), Some(NO_RESUME));
        assert!(list.rows[2].can_sleep && list.rows[2].sleep_reason.is_none());
        assert!(!list.rows[3].can_sleep, "already asleep");
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
                msg: None,
                title: None,
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
                }),
            );
        }
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(records);
        hub.agent_op(&s, &open(true, None), &AgentOp::Refresh, 7)
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
                item(LossKind::Shell, "Proj · terminal"),
                item(LossKind::Session, "phone"),
            ]
        );
        let v = serde_json::to_value(&plan.lose[1]).unwrap();
        assert_eq!(v, json!({"kind": "shell", "name": "Proj · terminal"}));
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
        hub.sleep_server(&s, "local", &plan).expect("sleep server");

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
        hub.sleep_server(&s, "local", &plan).expect("sleep server");
        assert!(f.stopped());
        assert_eq!(f.wait_control_connections(0, Duration::from_secs(5)), 0);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(hub.servers(), ["local"], "heard, with nothing on it");
        assert!(hub.live_sessions("local").is_empty());
        assert_eq!(hub.stoppable("local"), Err(super::NOT_RUNNING));
        assert!(hub.agent_panes("local", "session:old").is_empty());
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
        assert!(hub.sleep_server(&s, "local", &plan).is_err());
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
        assert_eq!(hub.live_sessions("local").len(), 1);
        assert_eq!(hub.stoppable("local"), Ok(()));
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
            &open(false, None),
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
            &open(false, None),
            &AgentOp::Rename("b".into(), "later".into()),
            7,
        )
        .expect("rename asleep");
        assert_eq!(f.requests_for("session.set_window_label").len(), 1);
        assert_eq!(state_of(&s, "b").unwrap().name, "later");
    }
}
