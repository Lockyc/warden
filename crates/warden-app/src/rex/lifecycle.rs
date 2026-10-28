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
/// [`RexHub::block_status`]).
pub fn agent_list(
    tab_id: &str,
    store: &AgentStore,
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
        if let Some(g) = &r.group {
            if !groups.iter().any(|x| x.name == *g) {
                groups.push(AgentGroupDto {
                    name: g.clone(),
                    folded: folded.contains(g),
                    amber: false,
                });
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
}

/// Project `tab_id`'s list as it stands: what `warden:agents` carries, and what the chrome pulls
/// (`agents_init`) for a tab it has no list for yet.
pub fn current_list(app: &AppHandle, tab_id: &str) -> Option<AgentListDto> {
    let store = app.try_state::<Mutex<AgentStore>>()?;
    let hub = app.try_state::<RexHub>()?;
    let dto = agent_list(tab_id, &lock(&store), |b| hub.block_status(b));
    Some(dto)
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
/// changing re-sends its project's list.
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
                        let project = app
                            .try_state::<Mutex<AgentStore>>()
                            .and_then(|s| lock(&s).project_of_block(&block_id));
                        if let Some(project) = project {
                            emit_agents(&app, &project);
                        }
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("warden: agent tabs won't follow Rex: {e}");
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
        let list = agent_list("t", &s.lock().unwrap(), |_| None);
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
        let list = agent_list("t", &s.lock().unwrap(), |_| None);
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
        let list = agent_list("t", &s.lock().unwrap(), |_| None);
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
        let list = agent_list("t", &s.lock().unwrap(), status);
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
