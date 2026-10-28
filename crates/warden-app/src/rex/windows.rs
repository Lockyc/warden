//! Agent windows and block status: the windows of every session the hub has `session.attach`ed,
//! and what the program in each of their terminal blocks last reported.
//!
//! A session's windows are its `session.view`, read at attach and again whenever
//! `session_view_changed` names a revision newer than the one held; `window_created`,
//! `window_closed` and `window_label_changed` apply in between, so a list follows a change before
//! the view that confirms it arrives. A window's block is its first terminal block.
//!
//! A block's status is its root OSC 7501 record (`program_status_changed` /
//! `program_status_removed`; child records are ignored) and its title (`title_changed`). Both are
//! pulled for every block of a session when it is attached, and for a block first seen in a later
//! view. Events and pulls are facts tagged with their connection's seq, so an older one never
//! overwrites a newer. Every block of a watched session is tracked, shown on screen or not.
//!
//! Views and pulls are read on their own worker threads, never under the state lock.

// The agent tab list is the first caller of these; until then only the tests read them.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Instant;

use rex_client::{ControlClient, Event, RexError};
use serde_json::{json, Value};

use super::{
    call_seq, str_field, terminal_options, view_blocks, HubEvent, Inner, RexHub, OPEN_BUDGET,
    TERMINAL_CREATOR, TERMINAL_FLAVOR,
};

const PROGRAM_STATUS: &str = "com.superlogical.terminal.program_status";
const TITLE: &str = "com.superlogical.terminal.title";

/// One window of a session, as a row of agent tabs shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWindow {
    pub window_id: String,
    /// The window's first terminal block; `None` until a view shows it.
    pub block_id: Option<String>,
    pub label: String,
}

/// An OSC 7501 `state`. A value Rex doesn't define reads as `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Blocked,
    Done,
    Error,
    Idle,
}

impl AgentState {
    fn parse(s: &str) -> AgentState {
        match s {
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "done" => AgentState::Done,
            "error" => AgentState::Error,
            _ => AgentState::Idle,
        }
    }
}

/// What a block's program last reported. A block with a title and no status record is `Idle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStatus {
    pub state: AgentState,
    pub msg: Option<String>,
    pub title: Option<String>,
}

/// One attached session's windows.
#[derive(Debug, Default)]
pub(super) struct SessionWindows {
    /// Whether a view has been applied; until then the entry only collects revisions heard.
    viewed: bool,
    /// The view's revision, and the highest a `session.view` has been asked for.
    revision: u64,
    asked: u64,
    /// The highest revision a `session_view_changed` named.
    heard: u64,
    /// The connection and response seq the view came from: window events at or below it are in it.
    gen: u64,
    seq: u64,
    list: Vec<AgentWindow>,
}

/// A value as of an event or response `seq` on connection `gen`.
#[derive(Debug, Default)]
struct Fact<T> {
    gen: u64,
    seq: u64,
    value: T,
}

impl<T> Fact<T> {
    /// Take `value` unless what's held is newer on the same connection; true if it was taken.
    fn set(&mut self, gen: u64, seq: u64, value: T) -> bool {
        if self.gen == gen && self.seq > seq {
            return false;
        }
        *self = Fact { gen, seq, value };
        true
    }
}

/// A block's root status record (`state`, `msg`) and its title.
#[derive(Debug, Default)]
pub(super) struct Tracked {
    record: Fact<Option<(AgentState, Option<String>)>>,
    title: Fact<Option<String>>,
}

impl Tracked {
    fn status(&self) -> Option<BlockStatus> {
        let title = self.title.value.clone();
        match &self.record.value {
            Some((state, msg)) => Some(BlockStatus {
                state: *state,
                msg: msg.clone(),
                title,
            }),
            None => title.map(|t| BlockStatus {
                state: AgentState::Idle,
                msg: None,
                title: Some(t),
            }),
        }
    }
}

impl RexHub {
    /// `session_id`'s windows on `server`, in `session.view` order. Empty for a session no pane
    /// has attached.
    pub fn windows(&self, server: &str, session_id: &str) -> Vec<AgentWindow> {
        self.inner
            .lock()
            .windows
            .get(&(server.to_owned(), session_id.to_owned()))
            .map(|w| w.list.clone())
            .unwrap_or_default()
    }

    /// What `block_id`'s program last reported, if anything.
    pub fn block_status(&self, block_id: &str) -> Option<BlockStatus> {
        self.inner
            .lock()
            .statuses
            .get(block_id)
            .and_then(Tracked::status)
    }

    /// A new window in `session_id` labelled `label`, with one terminal in `dir` typing `startup`
    /// and a newline. Ends by [`OPEN_BUDGET`], then `Unreachable`.
    pub fn new_window(
        &self,
        server: &str,
        session_id: &str,
        dir: &Path,
        startup: Option<&str>,
        label: &str,
    ) -> Result<AgentWindow, RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        match self
            .inner
            .new_window_until(server, session_id, dir, startup, label, deadline)
        {
            Err(RexError::Timeout) => Err(RexError::Unreachable),
            Err(_) if Instant::now() >= deadline => Err(RexError::Unreachable),
            r => r,
        }
    }

    /// Close `window_id` and every block in it. A window already gone is closed.
    pub fn close_window(
        &self,
        server: &str,
        session_id: &str,
        window_id: &str,
    ) -> Result<(), RexError> {
        let (client, _) = self.inner.conn(server)?;
        let args = json!({ "session_id": session_id, "window_id": window_id });
        match client.call("session.close_window", args) {
            Ok(_) => {}
            Err(e) if matches!(api_code(&e), Some("not_found" | "session_not_found")) => {}
            Err(e) => return Err(e),
        }
        self.inner.edit_windows(server, session_id, |list| {
            let before = list.len();
            list.retain(|w| w.window_id != window_id);
            list.len() != before
        });
        Ok(())
    }

    /// Relabel `window_id`. Rex trims the label; an empty one restores its default, which the
    /// list shows once Rex reports it.
    pub fn set_window_label(
        &self,
        server: &str,
        session_id: &str,
        window_id: &str,
        label: &str,
    ) -> Result<(), RexError> {
        let (client, _) = self.inner.conn(server)?;
        client.call(
            "session.set_window_label",
            json!({ "session_id": session_id, "window_id": window_id, "label": label }),
        )?;
        let label = label.trim();
        if !label.is_empty() {
            self.inner
                .edit_windows(server, session_id, |list| relabel(list, window_id, label));
        }
        Ok(())
    }
}

impl Inner {
    /// [`RexHub::new_window`]'s work, every call waiting until `deadline`.
    fn new_window_until(
        self: &Arc<Self>,
        server: &str,
        session_id: &str,
        dir: &Path,
        startup: Option<&str>,
        label: &str,
        deadline: Instant,
    ) -> Result<AgentWindow, RexError> {
        let (client, _) = self.conn_until(server, deadline)?;
        let (_, made) = client.call_seq_until(
            "session.new_window",
            json!({
                "session_id": session_id,
                "window_label": label,
                "layout": {"block": {"flavor": TERMINAL_FLAVOR, "options": terminal_options(dir, startup)}},
            }),
            deadline,
        )?;
        let window = AgentWindow {
            window_id: str_field(&made["window_id"], "session.new_window window_id")?,
            block_id: made["block_ids"][0].as_str().map(str::to_owned),
            label: label.to_owned(),
        };
        self.edit_windows(server, session_id, |list| {
            match list.iter_mut().find(|w| w.window_id == window.window_id) {
                Some(w) if w.block_id.is_none() => w.block_id = window.block_id.clone(),
                Some(_) => return false,
                None => list.push(window.clone()),
            }
            true
        });
        Ok(window)
    }

    /// Apply `edit` to a watched session's windows; report a change when it returns true.
    fn edit_windows(
        &self,
        server: &str,
        session_id: &str,
        edit: impl FnOnce(&mut Vec<AgentWindow>) -> bool,
    ) {
        let changed = {
            let mut st = self.lock();
            let key = (server.to_owned(), session_id.to_owned());
            st.windows.get_mut(&key).is_some_and(|w| edit(&mut w.list))
        };
        if changed {
            self.emit([windows_changed(server, session_id)]);
        }
    }

    /// Start watching `session_id`'s windows before it is attached, so a view change heard
    /// before the attach answers is not lost.
    pub(super) fn watch_windows(&self, server: &str, session_id: &str) {
        self.lock()
            .windows
            .entry((server.to_owned(), session_id.to_owned()))
            .or_default();
    }

    /// Stop watching a session whose attach failed, unless an earlier view keeps it watched.
    pub(super) fn unwatch_unviewed(&self, server: &str, session_id: &str) {
        let mut st = self.lock();
        let key = (server.to_owned(), session_id.to_owned());
        if st.windows.get(&key).is_some_and(|w| !w.viewed) {
            st.windows.remove(&key);
        }
    }

    /// Apply `view` (read as of `seq` on `client`) to `session_id`'s windows if it is newer than
    /// the one held and `client` is still `server`'s connection, then pull the status of its
    /// blocks — every one when `pull_all` (an attach), else those not yet tracked.
    pub(super) fn learn_view(
        self: &Arc<Self>,
        server: &str,
        client: &Arc<ControlClient>,
        session_id: &str,
        seq: u64,
        view: &Value,
        pull_all: bool,
    ) {
        let revision = view["revision"].as_u64().unwrap_or(0);
        let list = parse_windows(view);
        let mut refresh = false;
        let mut pull = Vec::new();
        let changed = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(gen) = st
                .conns
                .get(server)
                .filter(|c| Arc::ptr_eq(&c.client, client))
                .map(|c| c.gen)
            else {
                return;
            };
            let entry = st
                .windows
                .entry((server.to_owned(), session_id.to_owned()))
                .or_default();
            let changed = if entry.viewed && revision < entry.revision {
                false
            } else {
                let changed = !entry.viewed || entry.list != list;
                entry.viewed = true;
                entry.revision = revision;
                entry.asked = entry.asked.max(revision);
                entry.gen = gen;
                entry.seq = seq;
                entry.list = list;
                changed
            };
            if entry.heard > entry.asked {
                entry.asked = entry.heard;
                refresh = true;
            }
            for block in terminal_blocks(view) {
                let tracked = st.statuses.get(&block).is_some_and(|t| t.record.gen == gen);
                if pull_all || !tracked {
                    pull.push(block);
                }
            }
            changed
        };
        if changed {
            self.emit([windows_changed(server, session_id)]);
        }
        if refresh {
            self.spawn_refresh(server, client, session_id);
        }
        if !pull.is_empty() {
            let weak = Arc::downgrade(self);
            let (server, client, session) =
                (server.to_owned(), client.clone(), session_id.to_owned());
            thread::spawn(move || pull_status(weak, server, client, session, pull));
        }
    }

    /// Read `session_id`'s view again on a worker and apply it.
    pub(super) fn spawn_refresh(
        self: &Arc<Self>,
        server: &str,
        client: &Arc<ControlClient>,
        session_id: &str,
    ) {
        let weak = Arc::downgrade(self);
        let (server, client, session) = (server.to_owned(), client.clone(), session_id.to_owned());
        thread::spawn(move || {
            let args = json!({ "session_id": session });
            let read = call_seq(&client, "session.view", args, None);
            let Some(inner) = weak.upgrade() else {
                return;
            };
            match read {
                Ok((seq, view)) => inner.learn_view(&server, &client, &session, seq, &view, false),
                // Let the next view change ask again.
                Err(_) => {
                    let mut st = inner.lock();
                    if let Some(w) = st.windows.get_mut(&(server, session)) {
                        w.asked = w.revision;
                    }
                }
            }
        });
    }

    /// Record a pulled fact for `block`, if `client` is still `server`'s connection.
    fn record_pulled(
        &self,
        server: &str,
        client: &Arc<ControlClient>,
        block: &str,
        set: impl FnOnce(&mut Tracked, u64) -> bool,
    ) {
        let changed = {
            let mut st = self.lock();
            let Some(gen) = st
                .conns
                .get(server)
                .filter(|c| Arc::ptr_eq(&c.client, client))
                .map(|c| c.gen)
            else {
                return;
            };
            let tracked = st.statuses.entry(block.to_owned()).or_default();
            let before = tracked.status();
            set(tracked, gen) && tracked.status() != before
        };
        if changed {
            self.emit([HubEvent::StatusChanged {
                block_id: block.to_owned(),
            }]);
        }
    }
}

/// Pull `blocks`' root status record and title, one block after another.
fn pull_status(
    hub: Weak<Inner>,
    server: String,
    client: Arc<ControlClient>,
    session: String,
    blocks: Vec<String>,
) {
    for block in blocks {
        let args = json!({ "session_id": session, "block_id": block });
        if let Ok((seq, r)) = call_seq(&client, PROGRAM_STATUS, args.clone(), None) {
            let root = r["records"]
                .as_array()
                .and_then(|records| records.iter().find(|r| is_root(&r["id"])))
                .map(parse_record);
            let Some(inner) = hub.upgrade() else {
                return;
            };
            inner.record_pulled(&server, &client, &block, |t, gen| {
                t.record.set(gen, seq, root)
            });
        }
        if let Ok((seq, r)) = call_seq(&client, TITLE, args, None) {
            let title = non_empty(&r["title"]);
            let Some(inner) = hub.upgrade() else {
                return;
            };
            inner.record_pulled(&server, &client, &block, |t, gen| {
                t.title.set(gen, seq, title)
            });
        }
    }
}

/// Apply a window, view or block-status event from connection `gen` of `server`, reporting
/// changes into `out`. Returns the session whose view should be read again.
pub(super) fn apply_event(
    windows: &mut HashMap<(String, String), SessionWindows>,
    statuses: &mut HashMap<String, Tracked>,
    server: &str,
    gen: u64,
    ev: &Event,
    out: &mut Vec<HubEvent>,
) -> Option<String> {
    let session = ev.session_id.as_deref()?;
    let fields = &ev.payload;
    match ev.kind.as_str() {
        "window_created" | "window_closed" | "window_label_changed" | "session_view_changed" => {
            let w = windows.get_mut(&(server.to_owned(), session.to_owned()))?;
            if ev.kind == "session_view_changed" {
                let revision = fields["revision"].as_u64()?;
                w.heard = w.heard.max(revision);
                if w.viewed && revision > w.asked {
                    w.asked = revision;
                    return Some(session.to_owned());
                }
                return None;
            }
            if w.gen == gen && ev.seq <= w.seq {
                return None;
            }
            let window_id = fields["window_id"].as_str()?;
            let label = fields["label"].as_str().unwrap_or_default();
            let changed = match ev.kind.as_str() {
                "window_created" => match w.list.iter_mut().find(|x| x.window_id == window_id) {
                    Some(x) if x.label == label => false,
                    Some(x) => {
                        x.label = label.to_owned();
                        true
                    }
                    None => {
                        w.list.push(AgentWindow {
                            window_id: window_id.to_owned(),
                            block_id: None,
                            label: label.to_owned(),
                        });
                        true
                    }
                },
                "window_closed" => {
                    let before = w.list.len();
                    w.list.retain(|x| x.window_id != window_id);
                    w.list.len() != before
                }
                _ => relabel(&mut w.list, window_id, label),
            };
            if changed {
                out.push(windows_changed(server, session));
            }
        }
        "program_status_changed" | "program_status_removed" | "title_changed" => {
            if fields["creator_name"]
                .as_str()
                .is_some_and(|c| c != TERMINAL_CREATOR)
            {
                return None;
            }
            let block = fields["block_id"].as_str()?;
            let own = &fields["payload"];
            let tracked = statuses.entry(block.to_owned()).or_default();
            let before = tracked.status();
            match ev.kind.as_str() {
                "program_status_changed" if is_root(&own["id"]) => {
                    tracked
                        .record
                        .set(gen, ev.seq, Some(parse_record(&own["record"])));
                }
                // `ids` names the records removed; without it, all of them went.
                "program_status_removed"
                    if own["ids"]
                        .as_array()
                        .is_none_or(|ids| ids.iter().any(is_root)) =>
                {
                    tracked.record.set(gen, ev.seq, None);
                }
                "title_changed" => {
                    tracked.title.set(gen, ev.seq, non_empty(&own["title"]));
                }
                _ => {}
            }
            if tracked.status() != before {
                out.push(HubEvent::StatusChanged {
                    block_id: block.to_owned(),
                });
            }
        }
        "block_closed" => {
            let block = fields["block_id"].as_str()?;
            if statuses.remove(block).and_then(|t| t.status()).is_some() {
                out.push(HubEvent::StatusChanged {
                    block_id: block.to_owned(),
                });
            }
        }
        _ => {}
    }
    None
}

fn windows_changed(server: &str, session_id: &str) -> HubEvent {
    HubEvent::WindowsChanged {
        server: server.to_owned(),
        session_id: session_id.to_owned(),
    }
}

fn relabel(list: &mut [AgentWindow], window_id: &str, label: &str) -> bool {
    match list.iter_mut().find(|w| w.window_id == window_id) {
        Some(w) if w.label != label => {
            w.label = label.to_owned();
            true
        }
        _ => false,
    }
}

/// A view's windows, each with its first terminal block.
fn parse_windows(view: &Value) -> Vec<AgentWindow> {
    let windows = view["windows"].as_array().into_iter().flatten();
    windows
        .enumerate()
        .filter_map(|(i, w)| {
            Some(AgentWindow {
                window_id: w["window_id"].as_str()?.to_owned(),
                block_id: view_blocks(view, Some(i))
                    .find(|b| b["creator_name"].as_str() == Some(TERMINAL_CREATOR))
                    .and_then(|b| b["block_id"].as_str())
                    .map(str::to_owned),
                label: w["label"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// Every terminal block in a view.
fn terminal_blocks(view: &Value) -> Vec<String> {
    view_blocks(view, None)
        .filter(|b| b["creator_name"].as_str() == Some(TERMINAL_CREATOR))
        .filter_map(|b| b["block_id"].as_str().map(str::to_owned))
        .collect()
}

/// Whether a record id names the root record: Rex leaves it out (or empty) for the root.
fn is_root(id: &Value) -> bool {
    id.as_str().is_none_or(str::is_empty)
}

fn parse_record(record: &Value) -> (AgentState, Option<String>) {
    let state = AgentState::parse(record["state"].as_str().unwrap_or_default());
    (state, non_empty(&record["msg"]))
}

fn api_code(e: &RexError) -> Option<&str> {
    match e {
        RexError::Api { code, .. } => Some(code),
        _ => None,
    }
}

fn non_empty(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}
