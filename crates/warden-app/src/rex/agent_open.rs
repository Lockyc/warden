//! Opening a Rex tab's main pane, whose session's windows are its agent tabs: which agents are
//! live, which one to show, and — only when the user opens the pane — waking it.
//!
//! An open reconciles the pane's records (`agents::reconcile`) with its session's windows, or with
//! a lost session (Rex restarted, crashed, the Mac rebooted), which puts every resumable agent to
//! sleep. An automatic open stops there: it creates no session and no window and types nothing. A
//! user open shows the last-selected agent, waking it first when it is asleep, and a pane with
//! nothing live and nothing to wake gets a session typing `cmd`, as a plain Rex pane does. Each
//! live agent's key (`registry::agent_key`) is bound to its block, so size claims and exit checks
//! resolve per agent; the main pane's own key shows no block.

use std::sync::Mutex;
use std::time::Instant;

use rex_client::{Endpoint, RexError};

use super::agents::{self, AgentRecord, AgentStore, RecordState};
use super::lifecycle::settle;
use super::windows::AgentWindow;
use super::{api_error, budgeted, lock, PaneTarget, RexHub, OPEN_BUDGET};
use crate::registry::{agent_key, AgentsOpened, RexOpen};

impl RexHub {
    /// Open `req`'s agent pane, reading and writing its records in `store` (never held across a
    /// network call); a record that closes is stamped `now`. The whole open, waking included,
    /// shares one [`OPEN_BUDGET`] and holds the pane, so two opens never wake twice.
    pub fn open_agents(
        &self,
        store: &Mutex<AgentStore>,
        req: &RexOpen,
        now: u64,
    ) -> Result<AgentsOpened, RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        let _held = self.inner.hold_pane(&req.pane_key, Some(deadline))?;
        let opened = self.open_agents_until(store, req, now, deadline);
        budgeted(deadline, opened)
    }

    /// Let agent `agent_key` go if its program has finished ([`RexHub::block_exited`]): close its
    /// window, or, when that window was the session's last, end the session as a plain pane's
    /// exit does ([`RexHub::destroy_if_exited`]). Returns whether the agent was let go: false
    /// while it runs, when that can't be told, or for a key no open bound.
    pub fn end_agent_if_exited(&self, agent_key: &str) -> bool {
        let Some(pane) = self.inner.lock().agents.get(agent_key).cloned() else {
            return false;
        };
        let Ok(_held) = self.inner.hold_pane(&pane, None) else {
            return false;
        };
        if !self.block_exited(agent_key) {
            return false;
        }
        let (rec, block) = {
            let st = self.inner.lock();
            match (st.record(agent_key), st.blocks.get(agent_key)) {
                (Some(rec), Some(block)) => (rec.clone(), block.clone()),
                _ => return false,
            }
        };
        let windows = self.windows(&rec.server, &rec.session_id);
        let shows = |w: &AgentWindow| w.block_id.as_deref() == Some(block.as_str());
        if windows.iter().any(|w| !shows(w)) {
            if let Some(w) = windows.iter().find(|w| shows(w)) {
                if let Err(e) = self.close_window(&rec.server, &rec.session_id, &w.window_id) {
                    eprintln!("warden: couldn't close Rex window for {agent_key:?}: {e}");
                }
            }
        } else if self.inner.session_outlives_pane(agent_key) {
            self.inner.forget(&pane);
        } else if let Err(e) = self.inner.destroy(&pane) {
            eprintln!("warden: couldn't end Rex session for {pane:?}: {e}");
        }
        self.inner.lock().unbind_agent(agent_key);
        true
    }

    /// [`RexHub::open_agents`]' work, every call waiting until `deadline`.
    fn open_agents_until(
        &self,
        store: &Mutex<AgentStore>,
        req: &RexOpen,
        now: u64,
        deadline: Instant,
    ) -> Result<AgentsOpened, RexError> {
        let (project, server) = (req.pane_key.as_str(), req.server.as_str());
        let held = lock(store).records(project);
        let mut session = self.find_session_until(project, server, deadline)?;
        let mut records = match &session {
            Some((_, sid)) => {
                agents::reconcile(project, &held, &self.windows(server, sid), true, now)
            }
            None => agents::reconcile(project, &held, &[], false, now),
        };
        let choice = last_selected(&records);
        let mut woken = None;
        if !req.auto && !choice.is_some_and(|i| records[i].state == RecordState::Live) {
            let asleep = choice.filter(|&i| records[i].state == RecordState::Asleep);
            match asleep.filter(|&i| records[i].resume.is_some()) {
                Some(i) => {
                    self.wake_until(req, &mut session, &mut records[i], deadline)?;
                    woken = Some(i);
                }
                None if records.iter().any(|r| r.state == RecordState::Live) => {}
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
                    let windows = self.windows(server, &t.session_id);
                    records = agents::reconcile(project, &records, &windows, true, now);
                    session = Some((t.endpoint, t.session_id));
                }
            }
        }
        let shown = settle(&mut records, woken, req.auto);
        let opened = self.bind_agents(project, session.as_ref(), &records, shown);
        lock(store).merge(&held, records);
        Ok(opened)
    }

    /// Wake `record` (asleep or closed, with a resume command) in `req`'s pane: a window typing
    /// `apply_resume(req.resume, record.resume)` in `session`, or, with none, a session created
    /// for it, which `session` then names. The window keeps the record's name. The record goes
    /// live on that window; its group and position stay.
    pub(super) fn wake_until(
        &self,
        req: &RexOpen,
        session: &mut Option<(Endpoint, String)>,
        record: &mut AgentRecord,
        deadline: Instant,
    ) -> Result<(), RexError> {
        let (project, server) = (req.pane_key.as_str(), req.server.as_str());
        let Some(resume) = record.resume.as_deref() else {
            return Err(api_error("no_resume", super::lifecycle::NO_RESUME, 0));
        };
        let cmd = warden_config::apply_resume(req.resume.as_deref(), resume);
        let name = Some(record.name.as_str()).filter(|n| !n.is_empty());
        let (endpoint, sid, window) = match session.as_ref() {
            Some((endpoint, sid)) => {
                let w = self.inner.new_window_until(
                    server,
                    sid,
                    &req.dir,
                    Some(&cmd),
                    Some(name.unwrap_or(&req.label)),
                    deadline,
                )?;
                (endpoint.clone(), sid.clone(), w)
            }
            None => {
                let t = self.inner.ensure_until(
                    project,
                    server,
                    &req.dir,
                    Some(&cmd),
                    &req.label,
                    name,
                    deadline,
                )?;
                let w = self.window_of(server, &t.session_id, &t.block_id)?;
                (t.endpoint, t.session_id, w)
            }
        };
        record.state = RecordState::Live;
        record.window_id = Some(window.window_id);
        record.block_id = window.block_id;
        record.closed_at = None;
        *session = Some((endpoint, sid));
        Ok(())
    }

    /// The window of `session_id` holding `block_id`, which a session just created shows.
    pub(super) fn window_of(
        &self,
        server: &str,
        session_id: &str,
        block_id: &str,
    ) -> Result<AgentWindow, RexError> {
        self.windows(server, session_id)
            .into_iter()
            .find(|w| w.block_id.as_deref() == Some(block_id))
            .ok_or_else(|| api_error("no_terminal", "the new agent's window is missing", 0))
    }

    /// `pane_key`'s mapped session on `server` if it still exists there — attached, so its
    /// windows are current — else `None`. Never creates one.
    pub(super) fn find_session_until(
        &self,
        pane_key: &str,
        server: &str,
        deadline: Instant,
    ) -> Result<Option<(Endpoint, String)>, RexError> {
        let (client, endpoint) = self.inner.conn_until(server, deadline)?;
        let mapped = self.inner.lock().store.get(pane_key).cloned();
        let Some(rec) = mapped.filter(|r| r.server == server) else {
            return Ok(None);
        };
        match self
            .inner
            .attach(server, &client, &rec.session_id, Some(deadline))
        {
            Ok(_) => Ok(Some((endpoint, rec.session_id))),
            Err(RexError::Api { code, .. }) if code == "session_not_found" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Bind each live agent of `project` in `session` to its block (dropping the pane's earlier
    /// bindings and its own block), and say what the pane shows. With a session the pane's
    /// windows are followed as agent tabs from now on ([`RexHub::agent_panes`]).
    pub(super) fn bind_agents(
        &self,
        project: &str,
        session: Option<&(Endpoint, String)>,
        records: &[AgentRecord],
        shown: Option<usize>,
    ) -> AgentsOpened {
        let mut st = self.inner.lock();
        let stale: Vec<String> = st
            .agents
            .iter()
            .filter(|(_, pane)| *pane == project)
            .map(|(agent, _)| agent.clone())
            .collect();
        for agent in stale {
            st.unbind_agent(&agent);
        }
        st.blocks.remove(project);
        let Some((endpoint, session_id)) = session else {
            return AgentsOpened::default();
        };
        st.agent_panes.insert(project.to_owned());
        let mut live: Vec<&AgentRecord> = records
            .iter()
            .filter(|r| r.state == RecordState::Live && r.block_id.is_some())
            .collect();
        live.sort_by_key(|r| r.position);
        let mut agents = Vec::with_capacity(live.len());
        for r in live {
            let Some(block_id) = r.block_id.clone() else {
                continue;
            };
            let key = agent_key(project, &r.id);
            st.agents.insert(key.clone(), project.to_owned());
            st.blocks.insert(key, block_id.clone());
            agents.push((
                r.id.clone(),
                PaneTarget {
                    endpoint: endpoint.clone(),
                    session_id: session_id.clone(),
                    block_id,
                },
            ));
        }
        let selected = shown
            .map(|i| records[i].id.clone())
            .filter(|id| agents.iter().any(|(a, _)| a == id));
        AgentsOpened { agents, selected }
    }
}

/// The record a user open wakes when it is asleep: the one marked selected, else the first in
/// list order, never a closed one. Which record then shows is [`settle`]'s.
fn last_selected(records: &[AgentRecord]) -> Option<usize> {
    let open = |r: &AgentRecord| r.state != RecordState::Closed;
    records
        .iter()
        .position(|r| r.selected && open(r))
        .or_else(|| {
            records
                .iter()
                .enumerate()
                .filter(|(_, r)| open(r))
                .min_by_key(|(_, r)| r.position)
                .map(|(i, _)| i)
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use rex_client::fake::{FakeServer, Reply};
    use rex_client::{ClientInfo, Endpoint};
    use serde_json::{json, Value};

    use crate::registry::{agent_key, RexOpen};
    use crate::rex::agents::{AgentRecord, AgentStore, RecordState};
    use crate::rex::store::{PaneRecord, RexStore};
    use crate::rex::{PaneTarget, RexHub};

    pub(crate) fn hub(endpoint: Endpoint, dir: &tempfile::TempDir) -> (RexHub, PathBuf) {
        let path = dir.path().join("rex-sessions.json");
        let info = ClientInfo {
            kind: "app",
            name: "warden-test".into(),
            version: "0.0.0".into(),
        };
        (RexHub::for_test(path.clone(), info, endpoint), path)
    }

    pub(crate) fn map(path: &Path, session_id: &str) {
        RexStore::load(path.to_path_buf()).set(
            "t",
            Some(PaneRecord {
                server: "local".into(),
                session_id: session_id.into(),
            }),
        );
    }

    /// A `session.view` of `session_id` with one terminal block per `(window, block, label)`.
    pub(crate) fn view(session_id: &str, windows: &[(&str, &str, &str)]) -> Value {
        let windows: Vec<Value> = windows
            .iter()
            .map(|(w, b, label)| {
                json!({
                    "window_id": w, "label": label,
                    "layers": [{"layer_id": "layer:1", "kind": "tiled", "bounds": [0, 0, 1, 1],
                                "layout": {"rect": [0, 0, 1, 1], "block_id": b},
                                "blocks": [{"block_id": b,
                                            "creator_name": "com.superlogical.terminal",
                                            "flavor": "com.superlogical.terminal.shell"}]}],
                })
            })
            .collect();
        json!({"client_id": "client:fake-1",
               "view": {"revision": 1, "session_id": session_id, "windows": windows}})
    }

    /// A fake whose session `alive` attaches with `windows` (none alive: every attach is
    /// `session_not_found`), and whose `session.create` makes `session:new` with `window:new`
    /// holding `block:new`, attached as one window labelled `created`.
    pub(crate) fn fake(
        alive: Option<&'static str>,
        windows: Vec<(&'static str, &'static str, &'static str)>,
    ) -> FakeServer {
        let f = FakeServer::start();
        let listed: Vec<Value> = alive
            .into_iter()
            .map(|s| json!({"session_id": s, "label": "t · main"}))
            .collect();
        f.reply("session.attach_all", json!({ "sessions": listed }));
        f.reply(
            "session.create",
            json!({"session_id": "session:new", "initial_windows": [
                {"window_id": "window:new", "block_ids": ["block:new"], "revision": 1}
            ]}),
        );
        f.reply(
            "session.new_window",
            json!({"window_id": "window:woke", "block_ids": ["block:woke"], "revision": 2}),
        );
        f.on("session.attach", move |p, _| {
            let id = p["session_id"].as_str().unwrap_or_default();
            if id == "session:new" {
                Reply::ok(view(id, &[("window:new", "block:new", "created")]))
            } else if Some(id) == alive {
                Reply::ok(view(id, &windows))
            } else {
                Reply::error("session_not_found", "gone", 404)
            }
        });
        f
    }

    pub(crate) fn record(
        id: &str,
        state: RecordState,
        window: Option<&str>,
        resume: Option<&str>,
    ) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            project: "t".into(),
            name: id.into(),
            group: None,
            position: 0,
            resume: resume.map(Into::into),
            state,
            window_id: window.map(Into::into),
            block_id: window.map(|w| w.replace("window", "block")),
            closed_at: None,
            selected: false,
            forks_made: 0,
            forked_from: None,
            user_named: false,
            summary: None,
            subject: None,
        }
    }

    pub(crate) fn store(records: Vec<AgentRecord>) -> Mutex<AgentStore> {
        let mut s = AgentStore::default();
        for (i, mut r) in records.into_iter().enumerate() {
            r.position = i as u32;
            s.upsert(r);
        }
        Mutex::new(s)
    }

    pub(crate) fn open(auto: bool, resume: Option<&str>) -> RexOpen {
        RexOpen {
            pane_key: "t".into(),
            server: "local".into(),
            dir: PathBuf::from("/work/t"),
            startup: Some("agent".into()),
            label: "t · main".into(),
            token: 0,
            auto,
            agents: true,
            resume: resume.map(Into::into),
        }
    }

    pub(crate) fn state_of(s: &Mutex<AgentStore>, id: &str) -> Option<AgentRecord> {
        s.lock()
            .unwrap()
            .records("t")
            .into_iter()
            .find(|r| r.id == id)
    }

    /// Two resumable agents (`b` last selected) and one that never reported, all live in a
    /// session Rex lost when it restarted.
    fn before_restart() -> Mutex<AgentStore> {
        let mut b = record(
            "b",
            RecordState::Live,
            Some("window:2"),
            Some("claude --resume bbb"),
        );
        b.selected = true;
        store(vec![
            record(
                "a",
                RecordState::Live,
                Some("window:1"),
                Some("claude --resume aaa"),
            ),
            b,
            record("c", RecordState::Live, Some("window:3"), None),
        ])
    }

    #[test]
    fn auto_open_after_restart_creates_nothing() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = before_restart();

        let got = hub.open_agents(&s, &open(true, None), 7).expect("open");
        assert!(got.agents.is_empty(), "nothing live to show");
        assert_eq!(got.selected, None);
        assert!(f.requests_for("session.create").is_empty(), "no session");
        assert!(f.requests_for("session.new_window").is_empty(), "no window");
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Asleep);
        assert_eq!(state_of(&s, "b").unwrap().state, RecordState::Asleep);
        assert_eq!(
            state_of(&s, "c"),
            None,
            "no resume command: nothing can bring it back"
        );
    }

    #[test]
    fn user_open_wakes_last_selected_only() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = before_restart();

        let got = hub
            .open_agents(&s, &open(false, Some("claude-work")), 7)
            .expect("open");
        let creates = f.requests_for("session.create");
        assert_eq!(
            creates.len(),
            1,
            "the session comes back for the woken agent"
        );
        let win = &creates[0].payload["initial_windows"][0];
        assert_eq!(win["window_label"], "b", "the tab keeps its name");
        assert_eq!(
            win["layout"]["block"]["options"]["initial_input"],
            "claude-work --resume bbb\n"
        );
        assert!(f.requests_for("session.new_window").is_empty());

        let target = PaneTarget {
            endpoint: f.endpoint(),
            session_id: "session:new".into(),
            block_id: "block:new".into(),
        };
        assert_eq!(got.agents, vec![("b".to_string(), target)]);
        assert_eq!(got.selected.as_deref(), Some("b"));
        let b = state_of(&s, "b").unwrap();
        assert_eq!(b.state, RecordState::Live);
        assert_eq!(b.window_id.as_deref(), Some("window:new"));
        assert_eq!(b.block_id.as_deref(), Some("block:new"));
        assert!(b.selected);
        assert_eq!(
            state_of(&s, "a").unwrap().state,
            RecordState::Asleep,
            "only one wakes"
        );
    }

    #[test]
    fn user_open_wakes_into_a_live_session() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut b = record("b", RecordState::Asleep, None, Some("codex resume bbb"));
        b.selected = true;
        let s = store(vec![
            record("a", RecordState::Live, Some("window:1"), None),
            b,
        ]);

        let got = hub.open_agents(&s, &open(false, None), 7).expect("open");
        assert!(f.requests_for("session.create").is_empty());
        let made = f.requests_for("session.new_window");
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].payload["session_id"], "session:old");
        assert_eq!(made[0].payload["window_label"], "b");
        assert_eq!(
            made[0].payload["layout"]["block"]["options"]["initial_input"],
            "codex resume bbb\n"
        );
        let ids: Vec<&str> = got.agents.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(got.agents[1].1.block_id, "block:woke");
        assert_eq!(got.selected.as_deref(), Some("b"));
    }

    #[test]
    fn a_live_selected_agent_wakes_nothing() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record("a", RecordState::Live, Some("window:1"), None);
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Asleep, None, Some("claude --resume bbb")),
        ]);

        let got = hub.open_agents(&s, &open(false, None), 7).expect("open");
        assert!(f.requests_for("session.create").is_empty());
        assert!(f.requests_for("session.new_window").is_empty());
        assert_eq!(got.agents.len(), 1);
        assert_eq!(got.selected.as_deref(), Some("a"));
        assert_eq!(state_of(&s, "b").unwrap().state, RecordState::Asleep);
    }

    #[test]
    fn auto_open_attaches_every_live_window() {
        let f = fake(
            Some("session:old"),
            vec![
                ("window:1", "block:1", "one"),
                ("window:2", "block:2", "two"),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![]);

        let got = hub.open_agents(&s, &open(true, None), 7).expect("open");
        assert!(f.requests_for("session.create").is_empty());
        let blocks: Vec<&str> = got
            .agents
            .iter()
            .map(|(_, t)| t.block_id.as_str())
            .collect();
        assert_eq!(
            blocks,
            ["block:1", "block:2"],
            "windows made elsewhere are tabs too"
        );
        let records = s.lock().unwrap().records("t");
        assert_eq!(records.len(), 2);
        assert_eq!(got.selected.as_deref(), Some(records[0].id.as_str()));
        assert!(records[0].selected && !records[1].selected);
    }

    #[test]
    fn auto_open_keeps_an_asleep_last_selected_record_selected() {
        let f = fake(Some("session:old"), vec![("window:2", "block:2", "b")]);
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let mut a = record("a", RecordState::Asleep, None, Some("claude --resume aaa"));
        a.selected = true;
        let s = store(vec![
            a,
            record("b", RecordState::Live, Some("window:2"), None),
        ]);

        let got = hub.open_agents(&s, &open(true, None), 7).expect("open");
        assert!(
            f.requests_for("session.new_window").is_empty(),
            "wakes nothing"
        );
        assert_eq!(
            got.selected.as_deref(),
            Some("b"),
            "the live neighbour shows"
        );
        let a = state_of(&s, "a").unwrap();
        assert_eq!(a.state, RecordState::Asleep);
        assert!(a.selected, "the next user open wakes it");
        assert!(!state_of(&s, "b").unwrap().selected);

        hub.open_agents(&s, &open(false, None), 8)
            .expect("user open");
        assert_eq!(f.requests_for("session.new_window").len(), 1);
        assert_eq!(state_of(&s, "a").unwrap().state, RecordState::Live);
    }

    #[test]
    fn a_first_user_open_types_cmd_like_a_plain_rex_pane() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![]);

        let got = hub.open_agents(&s, &open(false, None), 7).expect("open");
        let creates = f.requests_for("session.create");
        assert_eq!(creates.len(), 1);
        let win = &creates[0].payload["initial_windows"][0];
        assert_eq!(
            win["layout"]["block"]["options"]["initial_input"],
            "agent\n"
        );
        assert!(win.get("window_label").is_none(), "Rex names it");
        let records = s.lock().unwrap().records("t");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, RecordState::Live);
        assert_eq!(records[0].name, "created");
        assert_eq!(got.agents.len(), 1);
        assert_eq!(got.agents[0].1.block_id, "block:new");
        assert_eq!(got.selected.as_deref(), Some(records[0].id.as_str()));
    }

    #[test]
    fn an_auto_open_never_creates_a_session_for_a_new_tab() {
        let f = fake(None, vec![]);
        let dir = tempfile::tempdir().unwrap();
        let (hub, _) = hub(f.endpoint(), &dir);
        let got = hub
            .open_agents(&store(vec![]), &open(true, None), 7)
            .expect("open");
        assert!(got.agents.is_empty());
        assert!(f.requests_for("session.create").is_empty());
    }

    #[test]
    fn agent_keys_size_and_exit_their_own_block() {
        let f = fake(
            Some("session:old"),
            vec![
                ("window:1", "block:1", "one"),
                ("window:2", "block:2", "two"),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![
            record("a", RecordState::Live, Some("window:1"), None),
            record(
                "b",
                RecordState::Live,
                Some("window:2"),
                Some("claude --resume bbb"),
            ),
        ]);
        hub.open_agents(&s, &open(true, None), 7).expect("open");
        let b = agent_key("t", "b");

        f.reply(
            "com.superlogical.terminal.process",
            json!({"last_exit": null}),
        );
        assert!(!hub.block_exited(&b));
        let asked = f.requests_for("com.superlogical.terminal.process");
        assert_eq!(asked[0].payload["block_id"], "block:2");
        assert!(!hub.end_agent_if_exited(&b), "still running");

        f.reply(
            "com.superlogical.terminal.process",
            json!({"last_exit": {"exit_code": 0}}),
        );
        assert!(hub.end_agent_if_exited(&b), "it exited");
        let left = hub.windows("local", "session:old");
        let closes = f.requests_for("session.close_window");
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].payload["window_id"], "window:2");
        assert!(
            f.requests_for("session.destroy").is_empty(),
            "another window holds it"
        );
        let ids: Vec<&str> = left.iter().map(|w| w.window_id.as_str()).collect();
        assert_eq!(ids, ["window:1"]);
        assert!(!hub.block_exited(&b), "let go of");
    }

    #[test]
    fn a_resume_reported_during_an_open_survives_it() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "a")]);
        let s = std::sync::Arc::new(store(vec![
            record("a", RecordState::Live, Some("window:1"), None),
            record("b", RecordState::Asleep, None, Some("claude --resume bbb")),
        ]));
        // The agent reports while the open is waiting on Rex, between its read and its write.
        let during = s.clone();
        f.on("session.attach", move |p, _| {
            let id = p["session_id"].as_str().unwrap_or_default();
            let mut st = during.lock().unwrap();
            assert!(st.report_resume("block:1", "claude --resume aaa"));
            Reply::ok(view(id, &[("window:1", "block:1", "renamed")]))
        });
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);

        hub.open_agents(&s, &open(true, None), 7).expect("open");
        let a = state_of(&s, "a").unwrap();
        assert_eq!(
            a.resume.as_deref(),
            Some("claude --resume aaa"),
            "the report is kept"
        );
        assert_eq!(a.name, "renamed", "the open's own change lands");
        assert!(a.selected);
        assert_eq!(state_of(&s, "b").unwrap().state, RecordState::Asleep);
    }

    #[test]
    fn the_last_agent_exiting_ends_the_session() {
        let f = fake(Some("session:old"), vec![("window:1", "block:1", "one")]);
        f.reply(
            "session.list_blocks",
            json!({"blocks": [{"block_id": "block:1", "placement": "placed"}]}),
        );
        let dir = tempfile::tempdir().unwrap();
        map(&dir.path().join("rex-sessions.json"), "session:old");
        let (hub, _) = hub(f.endpoint(), &dir);
        let s = store(vec![record("a", RecordState::Live, Some("window:1"), None)]);
        hub.open_agents(&s, &open(true, None), 7).expect("open");
        f.reply(
            "com.superlogical.terminal.process",
            json!({"last_exit": {"exit_code": 0}}),
        );
        assert!(hub.end_agent_if_exited(&agent_key("t", "a")), "exited");
        assert!(hub.windows("local", "session:old").is_empty());
        assert_eq!(f.requests_for("session.destroy").len(), 1);
        assert!(!hub.session_alive("t"));
    }
}
