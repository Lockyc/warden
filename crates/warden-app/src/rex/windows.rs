//! Agent windows and block status: the windows of every session the hub has `session.attach`ed,
//! and what the program in each of their terminal blocks last reported.
//!
//! A session's windows are its `session.view`, read at attach and again whenever
//! `session_view_changed` names a revision newer than the one held; `window_created`,
//! `window_closed` and `window_label_changed` apply in between, so a list follows a change before
//! the view that confirms it arrives. A window's block is its first terminal block.
//!
//! A block's status is its root OSC 7501 record (`program_status_changed` /
//! `program_status_removed`), the agent hook's text records ([`TEXTS`]; any other
//! child record is ignored) and its title (`title_changed`). All are pulled for every block of a
//! session when it is attached, and for a block first seen in a later view. Events and pulls are facts tagged with their connection's seq, so an older one never
//! overwrites a newer. Every block of a watched session is tracked, shown on screen or not.
//!
//! A window still on Rex's own label takes its block's title as its label once that block's
//! program reports a status and a title that isn't just the program's name
//! ([`Inner::name_windows`]) — so an agent tab is named for its conversation, and a name it was
//! given, by warden or the user, is never replaced.
//!
//! Views and pulls are read on their own worker threads, never under the state lock.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Instant;

use rex_client::{ControlClient, Event, RexError};
use serde_json::{json, Value};

use super::layout::Layout;
use super::{
    call_seq, str_field, terminal_options, view_blocks, HubEvent, Inner, RexHub, TERMINAL_CREATOR,
    TERMINAL_FLAVOR,
};

const PROGRAM_STATUS: &str = "com.superlogical.terminal.program_status";
const TITLE: &str = "com.superlogical.terminal.title";

/// One window of a session, as a row of agent tabs shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentWindow {
    pub window_id: String,
    /// The window's first terminal block; `None` until a view shows it.
    pub block_id: Option<String>,
    pub label: String,
    /// The window's first layer's split layout; `None` until a view shows it.
    pub layout: Option<Layout>,
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

/// The child records an agent's hook (`scripts/agent-hooks/claude-hook.sh`) keeps on its block,
/// one line each: the user's latest prompt, a short reply typed since it, and the agent's
/// latest reply.
const ASKED: &str = "warden/asked";
const ANSWERED: &str = "warden/answered";
const SAID: &str = "warden/said";
const TEXTS: [&str; 3] = [ASKED, ANSWERED, SAID];

/// What a block's program last reported. A block with no root status record is `Idle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStatus {
    pub state: AgentState,
    pub msg: Option<String>,
    pub title: Option<String>,
    /// The [`ASKED`] record's text.
    pub asked: Option<String>,
    /// The [`ANSWERED`] record's text.
    pub answered: Option<String>,
    /// The [`SAID`] record's text.
    pub said: Option<String>,
}

/// One attached session's windows.
#[derive(Debug, Default)]
pub(super) struct SessionWindows {
    /// Whether a view has been applied; until then the entry only collects revisions heard.
    pub(super) viewed: bool,
    /// The view's revision, and the highest a `session.view` has been asked for.
    revision: u64,
    asked: u64,
    /// The highest revision a `session_view_changed` named, on connection `heard_gen`.
    heard: u64,
    heard_gen: u64,
    /// The connection and response seq the view came from: window events at or below it are in it.
    gen: u64,
    seq: u64,
    pub(super) list: Vec<AgentWindow>,
    /// The view's `active_window_id`.
    pub(super) active: Option<String>,
}

impl SessionWindows {
    /// The held view's `(connection gen, revision)` — ordered across reconnects.
    pub(super) fn revision(&self) -> (u64, u64) {
        (self.gen, self.revision)
    }
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

/// A block's root OSC 7501 record: its `state`, `msg`, and the `app` and process (`pid`) that
/// reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    state: AgentState,
    msg: Option<String>,
    app: Option<String>,
    pid: Option<u64>,
}

/// A block's title and the process (`pid`) that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Title {
    text: String,
    pid: Option<u64>,
}

/// A block's root status record, its title, and the hook's [`TEXTS`].
#[derive(Debug, Default)]
pub(super) struct Tracked {
    record: Fact<Option<Record>>,
    title: Fact<Option<Title>>,
    asked: Fact<Option<String>>,
    answered: Fact<Option<String>>,
    said: Fact<Option<String>>,
}

impl Tracked {
    fn status(&self) -> Option<BlockStatus> {
        let title = self.title.value.as_ref().map(|t| t.text.clone());
        let asked = self.asked.value.clone();
        let answered = self.answered.value.clone();
        let said = self.said.value.clone();
        let (state, msg) = match &self.record.value {
            Some(r) => (r.state, r.msg.clone()),
            None if title.is_none() && asked.is_none() && answered.is_none() && said.is_none() => {
                return None
            }
            None => (AgentState::Idle, None),
        };
        Some(BlockStatus {
            state,
            msg,
            title,
            asked,
            answered,
            said,
        })
    }

    /// The hook's text record `id` names, if it is one.
    fn text_record(&mut self, id: &str) -> Option<&mut Fact<Option<String>>> {
        match id {
            ASKED => Some(&mut self.asked),
            ANSWERED => Some(&mut self.answered),
            SAID => Some(&mut self.said),
            _ => None,
        }
    }

    /// The title a window still on Rex's label should take: one set by the process that reported
    /// the status record — not the shell that ran it — less its leading status glyph, other than
    /// the program's own name ("✳ Claude Code" from `claude-code`).
    fn name(&self) -> Option<&str> {
        let record = self.record.value.as_ref()?;
        let title = self.title.value.as_ref()?;
        if record.pid.is_none() || title.pid != record.pid {
            return None;
        }
        let text = unglyphed(&title.text);
        (!text.is_empty() && !self.is_own_name(text)).then_some(text)
    }

    /// True if `label` is only the reporting program's own name — a window given it is still
    /// waiting for a real one.
    fn is_own_name(&self, label: &str) -> bool {
        let label = unglyphed(label).to_lowercase().replace(' ', "-");
        self.record
            .value
            .as_ref()
            .and_then(|r| r.app.as_deref())
            .is_some_and(|app| label == app.to_lowercase())
    }
}

/// `title` without the status glyph a program leads it with (Claude Code's `✳`, and its
/// spinner frames while working) or surrounding whitespace. ASCII is kept, so `[WIP] x` and
/// `~/dir` survive.
fn unglyphed(title: &str) -> &str {
    title
        .trim_start_matches(|c: char| c.is_whitespace() || (!c.is_ascii() && !c.is_alphanumeric()))
        .trim_end()
}

/// Rex's own label for a window it named: `Window` and the last 8 digits of its id.
fn rex_label(window_id: &str) -> Option<String> {
    let tail = window_id.get(window_id.len().checked_sub(8)?..)?;
    Some(format!("Window {tail}"))
}

impl RexHub {
    /// `session_id`'s windows on `server`, in `session.view` order, less those closing. Empty for
    /// a session no pane has attached.
    pub fn windows(&self, server: &str, session_id: &str) -> Vec<AgentWindow> {
        let st = self.inner.lock();
        let closing = |w: &AgentWindow| {
            st.closing
                .contains(&(server.to_owned(), w.window_id.clone()))
        };
        st.windows
            .get(&(server.to_owned(), session_id.to_owned()))
            .map(|w| w.list.iter().filter(|w| !closing(w)).cloned().collect())
            .unwrap_or_default()
    }

    /// Leave `window_id` out of [`RexHub::windows`] until its close answers.
    pub(super) fn mark_closing(&self, server: &str, window_id: &str) {
        self.inner
            .lock()
            .closing
            .insert((server.to_owned(), window_id.to_owned()));
    }

    /// `session_id`'s windows on `server` as Rex has them now (`session.view`), without following
    /// the session. Empty once the session is gone.
    pub fn read_windows(
        &self,
        server: &str,
        session_id: &str,
    ) -> Result<Vec<AgentWindow>, RexError> {
        let (client, _) = self.inner.conn(server)?;
        match client.call("session.view", json!({ "session_id": session_id })) {
            Ok(view) => Ok(parse_windows(&view)),
            Err(e) if api_code(&e) == Some("session_not_found") => Ok(Vec::new()),
            Err(e) => Err(e),
        }
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
    /// and a newline. Ends by [`super::OPEN_BUDGET`], then `Unreachable`. The lifecycle runs
    /// [`Inner::new_window_until`] inside its own budget.
    #[cfg(test)]
    pub fn new_window(
        &self,
        server: &str,
        session_id: &str,
        dir: &Path,
        startup: Option<&str>,
        label: &str,
    ) -> Result<AgentWindow, RexError> {
        let deadline = Instant::now() + super::OPEN_BUDGET;
        let made =
            self.inner
                .new_window_until(server, session_id, dir, startup, Some(label), deadline);
        super::budgeted(deadline, made)
    }

    /// Close `window_id` and every block in it. A window already gone is closed.
    pub fn close_window(
        &self,
        server: &str,
        session_id: &str,
        window_id: &str,
    ) -> Result<(), RexError> {
        self.close_window_by(server, session_id, window_id, None)
    }

    /// [`RexHub::close_window`] waiting only until `deadline`, for a caller with a budget.
    pub(super) fn close_window_until(
        &self,
        server: &str,
        session_id: &str,
        window_id: &str,
        deadline: Instant,
    ) -> Result<(), RexError> {
        self.close_window_by(server, session_id, window_id, Some(deadline))
    }

    fn close_window_by(
        &self,
        server: &str,
        session_id: &str,
        window_id: &str,
        deadline: Option<Instant>,
    ) -> Result<(), RexError> {
        self.mark_closing(server, window_id);
        let closed = self.inner.conn(server).and_then(|(client, _)| {
            let args = json!({ "session_id": session_id, "window_id": window_id });
            match deadline {
                Some(d) => client
                    .call_seq_until("session.close_window", args, d)
                    .map(|_| ()),
                None => client.call("session.close_window", args).map(|_| ()),
            }
        });
        let closed = match closed {
            Err(e) if matches!(api_code(&e), Some("not_found" | "session_not_found")) => Ok(()),
            r => r,
        };
        if closed.is_ok() {
            self.inner.edit_windows(server, session_id, |list| {
                let before = list.len();
                list.retain(|w| w.window_id != window_id);
                list.len() != before
            });
        }
        self.inner
            .lock()
            .closing
            .remove(&(server.to_owned(), window_id.to_owned()));
        closed
    }

    /// Relabel `window_id`, waiting only until `deadline`. Rex trims the label; an empty one
    /// restores its default, which the list shows once Rex reports it.
    pub(super) fn set_window_label_until(
        &self,
        server: &str,
        session_id: &str,
        window_id: &str,
        label: &str,
        deadline: Instant,
    ) -> Result<(), RexError> {
        self.inner
            .set_window_label(server, session_id, window_id, label, Some(deadline))
    }
}

impl Inner {
    /// [`RexHub::set_window_label`]'s work.
    fn set_window_label(
        self: &Arc<Self>,
        server: &str,
        session_id: &str,
        window_id: &str,
        label: &str,
        deadline: Option<Instant>,
    ) -> Result<(), RexError> {
        let (client, _) = self.conn(server)?;
        let args = json!({ "session_id": session_id, "window_id": window_id, "label": label });
        match deadline {
            Some(d) => client
                .call_seq_until("session.set_window_label", args, d)
                .map(|_| ())?,
            None => client.call("session.set_window_label", args).map(|_| ())?,
        }
        let label = label.trim();
        if !label.is_empty() {
            self.edit_windows(server, session_id, |list| relabel(list, window_id, label));
        }
        Ok(())
    }

    /// Relabel each of `server`'s windows still on Rex's own label, or on its program's own name,
    /// whose block has a name to give it ([`Tracked::name`]), each on its own worker. A failed
    /// relabel is tried again at the block's next status change.
    pub(super) fn name_windows(self: &Arc<Self>, server: &str) {
        let due: Vec<(String, String, String)> = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let mut due = Vec::new();
            for ((s, session), w) in &st.windows {
                if s != server {
                    continue;
                }
                for win in &w.list {
                    if st.naming.contains(&win.window_id) {
                        continue;
                    }
                    let Some(tracked) = win.block_id.as_ref().and_then(|b| st.statuses.get(b))
                    else {
                        continue;
                    };
                    if rex_label(&win.window_id).as_deref() != Some(win.label.as_str())
                        && !tracked.is_own_name(&win.label)
                    {
                        continue;
                    }
                    let name = tracked.name();
                    if let Some(name) = name {
                        due.push((session.clone(), win.window_id.clone(), name.to_owned()));
                    }
                }
            }
            st.naming.extend(due.iter().map(|(_, w, _)| w.clone()));
            due
        };
        for (session, window, name) in due {
            let weak = Arc::downgrade(self);
            let server = server.to_owned();
            thread::spawn(move || {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if let Err(e) = inner.set_window_label(&server, &session, &window, &name, None) {
                    log::warn!("Rex {server}: couldn't name {window} {name:?}: {e}");
                }
                inner.lock().naming.remove(&window);
            });
        }
    }

    /// [`RexHub::new_window`]'s work, every call waiting until `deadline`. Without a `label` Rex
    /// names the window, and the list shows that name once Rex reports it.
    pub(super) fn new_window_until(
        self: &Arc<Self>,
        server: &str,
        session_id: &str,
        dir: &Path,
        startup: Option<&str>,
        label: Option<&str>,
        deadline: Instant,
    ) -> Result<AgentWindow, RexError> {
        let (client, _) = self.conn_until(server, deadline)?;
        let mut args = json!({
            "session_id": session_id,
            "layout": {"block": {"flavor": TERMINAL_FLAVOR, "options": terminal_options(dir, startup)}},
        });
        if let Some(label) = label {
            args["window_label"] = json!(label);
        }
        let (_, made) = client.call_seq_until("session.new_window", args, deadline)?;
        let window = AgentWindow {
            window_id: str_field(&made["window_id"], "session.new_window window_id")?,
            block_id: made["block_ids"][0].as_str().map(str::to_owned),
            label: label.unwrap_or_default().to_owned(),
            layout: None,
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
            let changed = st.windows.get_mut(&key).is_some_and(|w| edit(&mut w.list));
            if changed {
                st.bind_columns(server, session_id);
            }
            changed
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

    /// Whether a view of `session_id` is held, so its windows are current without attaching.
    pub(super) fn watching(&self, server: &str, session_id: &str) -> bool {
        self.lock()
            .windows
            .get(&(server.to_owned(), session_id.to_owned()))
            .is_some_and(|w| w.viewed)
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
        let active = view["active_window_id"].as_str().map(str::to_owned);
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
            // Revisions order views only within one connection: a restarted Rex counts a restored
            // session's from zero again, so a view on a new connection replaces the held one.
            let new_conn = entry.gen != gen;
            let changed = if entry.viewed && !new_conn && revision < entry.revision {
                false
            } else {
                let changed = !entry.viewed || entry.list != list || entry.active != active;
                entry.viewed = true;
                entry.revision = revision;
                entry.asked = if new_conn {
                    revision
                } else {
                    entry.asked.max(revision)
                };
                entry.gen = gen;
                entry.seq = seq;
                entry.list = list;
                entry.active = active;
                changed
            };
            if entry.heard_gen == gen && entry.heard > entry.asked {
                entry.asked = entry.heard;
                refresh = true;
            }
            for block in terminal_blocks(view) {
                let tracked = st.statuses.get(&block).is_some_and(|t| t.record.gen == gen);
                if pull_all || !tracked {
                    pull.push(block);
                }
            }
            if changed {
                st.bind_columns(server, session_id);
            }
            changed
        };
        if changed {
            self.emit([windows_changed(server, session_id)]);
            self.name_windows(server);
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
        self: &Arc<Self>,
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
            let named = tracked.name().map(str::to_owned);
            let taken = set(tracked, gen);
            (
                taken && tracked.status() != before,
                taken && tracked.name() != named.as_deref(),
            )
        };
        if changed.0 {
            self.emit([HubEvent::StatusChanged {
                block_id: block.to_owned(),
            }]);
        }
        if changed.0 || changed.1 {
            self.name_windows(server);
        }
    }
}

/// Pull `blocks`' root status record, the hook's text records and title, one block after another.
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
            let records = r["records"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let root = records.iter().find(|r| is_root(&r["id"])).map(parse_record);
            let text = |id: &str| {
                records
                    .iter()
                    .find(|r| r["id"].as_str() == Some(id))
                    .and_then(|r| non_empty(&r["msg"]))
            };
            let texts = TEXTS.map(|id| (id, text(id)));
            let Some(inner) = hub.upgrade() else {
                return;
            };
            inner.record_pulled(&server, &client, &block, |t, gen| {
                let mut changed = t.record.set(gen, seq, root);
                for (id, value) in texts {
                    if let Some(fact) = t.text_record(id) {
                        changed |= fact.set(gen, seq, value);
                    }
                }
                changed
            });
        }
        if let Ok((seq, r)) = call_seq(&client, TITLE, args, None) {
            let title = parse_title(&r);
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
                if w.heard_gen != gen {
                    (w.heard_gen, w.heard) = (gen, 0);
                }
                w.heard = w.heard.max(revision);
                if w.viewed && (w.gen != gen || revision > w.asked) {
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
                            layout: None,
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
                "program_status_changed" => {
                    let text = non_empty(&own["record"]["msg"]);
                    if let Some(fact) = own["id"].as_str().and_then(|id| tracked.text_record(id)) {
                        fact.set(gen, ev.seq, text);
                    }
                }
                // `ids` names the records removed; without it, all of them went.
                "program_status_removed" => {
                    let ids = own["ids"].as_array();
                    let gone =
                        |want: &dyn Fn(&Value) -> bool| ids.is_none_or(|ids| ids.iter().any(want));
                    if gone(&is_root) {
                        tracked.record.set(gen, ev.seq, None);
                    }
                    for id in TEXTS {
                        if gone(&|v| v.as_str() == Some(id)) {
                            if let Some(fact) = tracked.text_record(id) {
                                fact.set(gen, ev.seq, None);
                            }
                        }
                    }
                }
                "title_changed" => {
                    tracked.title.set(gen, ev.seq, parse_title(own));
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
                layout: Layout::parse(&w["layers"][0]["layout"]),
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

/// Whether a record id names the root record: `""` in `program_status_changed` and
/// `program_status_removed`, left out in `program_status`'s records.
fn is_root(id: &Value) -> bool {
    id.as_str().is_none_or(str::is_empty)
}

fn parse_record(record: &Value) -> Record {
    Record {
        state: AgentState::parse(record["state"].as_str().unwrap_or_default()),
        msg: non_empty(&record["msg"]),
        app: non_empty(&record["app"]),
        pid: record["owner"]["pid"].as_u64(),
    }
}

/// A `title_changed` payload's or a title pull's title, if it isn't empty.
fn parse_title(v: &Value) -> Option<Title> {
    Some(Title {
        text: non_empty(&v["title"])?,
        pid: v["owner"]["pid"].as_u64(),
    })
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
