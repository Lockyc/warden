//! A Rex tab's scratch column: its split pane's one Rex session, whose windows are the column's
//! scratch tabs and each window's split tree the panes stacked in it.
//!
//! The column keeps the split pane's key (`registry::secondary_id`), so its store record and the
//! Sleep and ☠ paths are a split pane's. Each block of each window is bound under [`block_key`] —
//! the key its size claims name — and rebound whenever the session's windows are relearned
//! ([`State::bind_columns`]). The config stack shapes the session only when warden creates it;
//! after that Rex holds the layout. A session Rex restored after a restart has each stack pane's
//! `cmd` retyped only while its first window still has the stack's shape.

use std::path::Path;
use std::time::{Duration, Instant};

use rex_client::{Endpoint, RexError};
use serde_json::json;

use super::layout::{stack_layout, Divider, Layout};
use super::store::PaneRecord;
use super::windows::AgentWindow;
use super::{api_error, budgeted, str_field, RexHub, State, OPEN_BUDGET};
use crate::registry::{tab_of_secondary, RexOpen};

/// The longest a change to an open column (new scratch tab, split, resize, close) waits on Rex.
/// Unlike an open it runs on a worker, so nothing waits on it but the change itself; a request
/// Rex has been sent is carried out however late it answers, so giving up sooner reports as failed
/// a change that then appears. Rex can stall every request for 10–20 s at a time (its macOS
/// feedback forum: "Requests stall about 10 s right after new_window or new_split"). A timeout
/// stays [`RexError::Timeout`]: the server answered the connection, it just didn't reply.
pub const CHANGE_BUDGET: Duration = Duration::from_secs(30);

/// Which side of a scratch block a split stacks its new shell on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSide {
    Above,
    Below,
}

impl StackSide {
    /// `session.new_split`'s `side`: `before` is the top part of a vertical split.
    fn wire(self) -> &'static str {
        match self {
            StackSide::Above => "before",
            StackSide::Below => "after",
        }
    }
}

/// One scratch tab: a window of the column's session, and its split tree.
#[derive(Debug, Clone, PartialEq)]
pub struct ScratchWindow {
    pub window_id: String,
    pub label: String,
    pub layout: Layout,
}

/// A column's session as Rex has it: its windows in view order, and the active one.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnOpened {
    pub endpoint: Endpoint,
    pub session_id: String,
    pub windows: Vec<ScratchWindow>,
    pub active: Option<String>,
    /// The view it was read from, as `(connection gen, view revision)`: ordered, so a snapshot
    /// read before a newer one can be told apart when it lands after it.
    pub revision: (u64, u64),
}

/// The size and claim key of `block_id` in the column `column_key`.
pub(crate) fn block_key(column_key: &str, block_id: &str) -> String {
    format!("{column_key}#{block_id}")
}

/// The column a [`block_key`] belongs to, or `None` for any other key. Rex block ids hold no
/// `#`, so the last one splits the key.
pub(super) fn column_of(key: &str) -> Option<&str> {
    let (column, _) = key.rsplit_once('#')?;
    tab_of_secondary(column).map(|_| column)
}

/// Every block of `windows`, window by window in layout order; a window whose layout no view has
/// shown yet counts its first terminal block.
fn window_blocks(windows: &[AgentWindow]) -> Vec<String> {
    windows
        .iter()
        .flat_map(|w| match (&w.layout, &w.block_id) {
            (Some(l), _) => l.blocks(),
            (None, Some(b)) => vec![b.clone()],
            (None, None) => Vec::new(),
        })
        .collect()
}

/// `windows` as scratch tabs. A window known only from an event shows its one block until a view
/// gives its layout; one with no block known yet isn't a tab until then.
fn scratch_windows(windows: &[AgentWindow]) -> Vec<ScratchWindow> {
    windows
        .iter()
        .filter_map(|w| {
            let layout = w
                .layout
                .clone()
                .or_else(|| w.block_id.as_deref().map(Layout::single))?;
            Some(ScratchWindow {
                window_id: w.window_id.clone(),
                label: w.label.clone(),
                layout,
            })
        })
        .collect()
}

impl State {
    /// Bind every block of `server`'s session `session_id` under each column mapped to it, and drop
    /// a column's keys (and their sizes) for blocks it no longer holds.
    pub(super) fn bind_columns(&mut self, server: &str, session_id: &str) {
        let columns: Vec<String> = self
            .store
            .panes_on(server)
            .into_iter()
            .filter(|(k, s)| s == session_id && tab_of_secondary(k).is_some())
            .map(|(k, _)| k)
            .collect();
        if columns.is_empty() {
            return;
        }
        let key = (server.to_owned(), session_id.to_owned());
        let blocks = self
            .windows
            .get(&key)
            .map(|w| window_blocks(&w.list))
            .unwrap_or_default();
        for c in &columns {
            let ours = |k: &str| column_of(k) == Some(c.as_str());
            self.blocks.retain(|k, b| !ours(k) || blocks.contains(b));
            for b in &blocks {
                self.blocks.insert(block_key(c, b), b.clone());
            }
            let bound = &self.blocks;
            self.requested
                .retain(|k, _| !ours(k) || bound.contains_key(k));
        }
    }
}

impl RexHub {
    /// Open `req`'s scratch column: its mapped session if that still exists — retyping the stack's
    /// commands when Rex restored it after a restart — else a new one laid out as `req.stack`
    /// ([`stack_layout`]; an empty stack is one bare shell). The whole open shares one
    /// [`OPEN_BUDGET`] and holds the pane.
    pub fn open_column(&self, req: &RexOpen) -> Result<ColumnOpened, RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        let _held = self.inner.hold_pane(&req.pane_key, Some(deadline))?;
        let opened = self.open_column_until(req, deadline);
        budgeted(deadline, opened)
    }

    /// The column whose session is `server`'s `session_id`, as last heard; `None` while no view
    /// of it is held or the server isn't connected.
    pub fn column(&self, server: &str, session_id: &str) -> Option<ColumnOpened> {
        let st = self.inner.lock();
        let endpoint = st.conns.get(server)?.endpoint.clone();
        let w = st
            .windows
            .get(&(server.to_owned(), session_id.to_owned()))
            .filter(|w| w.viewed)?;
        Some(ColumnOpened {
            endpoint,
            session_id: session_id.to_owned(),
            windows: scratch_windows(&w.list),
            active: w.active.clone(),
            revision: w.revision(),
        })
    }

    /// A new scratch tab in `column_key`'s session: a window with one bare shell in `dir`, named
    /// by Rex. Returns its window id.
    pub fn column_new_tab(&self, column_key: &str, dir: &Path) -> Result<String, RexError> {
        let deadline = Instant::now() + CHANGE_BUDGET;
        let _held = self.inner.hold_pane(column_key, Some(deadline))?;
        let rec = self.column_record(column_key)?;
        self.inner
            .new_window_until(&rec.server, &rec.session_id, dir, None, None, deadline)
            .map(|w| w.window_id)
    }

    /// Split `block_id` of `column_key`'s session: a bare shell in `dir` stacked on `side` of it,
    /// half its height, focused. Returns the new block's id, bound at once; the window's layout
    /// follows with Rex's next view.
    pub fn column_split(
        &self,
        column_key: &str,
        block_id: &str,
        side: StackSide,
        dir: &Path,
    ) -> Result<String, RexError> {
        let deadline = Instant::now() + CHANGE_BUDGET;
        let _held = self.inner.hold_pane(column_key, Some(deadline))?;
        let rec = self.column_record(column_key)?;
        let split = || {
            let (client, _) = self.inner.conn_with(&rec.server, deadline, false)?;
            let args = json!({
                "session_id": rec.session_id,
                "anchor_id": block_id,
                "direction": "vertical",
                "side": side.wire(),
                "ratio": 0.5,
                "layout": stack_layout(&[], dir),
                "focus": true,
            });
            let (_, made) = client.call_seq_until("session.new_split", args, deadline)?;
            str_field(&made["block_ids"][0], "session.new_split block id")
        };
        let new = split()?;
        self.inner
            .lock()
            .blocks
            .insert(block_key(column_key, &new), new.clone());
        Ok(new)
    }

    /// Move divider `d` of `column_key`'s session to `ratio` of its split.
    pub fn column_resize(&self, column_key: &str, d: &Divider, ratio: f64) -> Result<(), RexError> {
        let deadline = Instant::now() + CHANGE_BUDGET;
        let _held = self.inner.hold_pane(column_key, Some(deadline))?;
        let rec = self.column_record(column_key)?;
        let resize = || {
            let (client, _) = self.inner.conn_with(&rec.server, deadline, false)?;
            let args = json!({
                "session_id": rec.session_id,
                "direction": d.dir,
                "before_block_id": d.before_block,
                "after_block_id": d.after_block,
                "ratio": ratio,
            });
            client.call_seq_until("session.resize_pane", args, deadline)
        };
        resize().map(|_| ())
    }

    /// Close `block_id` of `column_key`'s session, ending the program in it — one that exited, or
    /// a pane the user closed — so its window reflows (or closes, when it was the window's last).
    /// Rex keeps a session whose last window closes, empty; [`Self::column_fill`] refills it. A
    /// column already unmapped — its session ended by ☠ racing this close — has nothing left to
    /// close.
    pub fn column_close_block(&self, column_key: &str, block_id: &str) -> Result<(), RexError> {
        let _held = self.inner.hold_pane(column_key, None)?;
        let Ok(rec) = self.column_record(column_key) else {
            log::debug!("Rex block {block_id:?} of {column_key:?}: its column already ended");
            return Ok(());
        };
        let deadline = Instant::now() + CHANGE_BUDGET;
        let (client, _) = self.inner.conn_with(&rec.server, deadline, false)?;
        let args = json!({ "session_id": rec.session_id, "block_id": block_id });
        match client.call_seq_until("block.close", args, deadline) {
            Ok(_) => {}
            Err(RexError::Api { code, .. }) if code == "not_found" => {}
            Err(e) => return Err(e),
        }
        self.unbind_blocks(column_key, &[block_id]);
        Ok(())
    }

    /// Close scratch tab `window_id` of `column_key`'s session and every block in it, ending their
    /// programs. The last one leaves the session empty for [`Self::column_fill`].
    pub fn column_close_tab(&self, column_key: &str, window_id: &str) -> Result<(), RexError> {
        let _held = self.inner.hold_pane(column_key, None)?;
        let rec = self.column_record(column_key)?;
        let window: Vec<AgentWindow> = self
            .windows(&rec.server, &rec.session_id)
            .into_iter()
            .filter(|w| w.window_id == window_id)
            .collect();
        let blocks = window_blocks(&window);
        let blocks: Vec<&str> = blocks.iter().map(String::as_str).collect();
        let deadline = Instant::now() + CHANGE_BUDGET;
        self.close_window_until(&rec.server, &rec.session_id, window_id, deadline)?;
        self.unbind_blocks(column_key, &blocks);
        Ok(())
    }

    /// Give `column_key`'s session a fresh scratch tab — one bare shell in `dir` — when it holds
    /// no block, as a scratch terminal respawns in place of its last shell: the column never
    /// sits empty, however its last tab went (×, ⌘W, its program exiting, or another client).
    /// Returns the new window's id; `None` when the session still holds a block, or has no
    /// mapping (☠ ended it).
    pub fn column_fill(&self, column_key: &str, dir: &Path) -> Result<Option<String>, RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        let _held = self.inner.hold_pane(column_key, Some(deadline))?;
        let Ok(rec) = self.column_record(column_key) else {
            return Ok(None);
        };
        let filled = self.fill_until(column_key, &rec, dir, deadline);
        budgeted(deadline, filled)
    }

    /// [`Self::column_fill`]'s work, under the caller's hold, waiting until `deadline`. Rex's
    /// own block list decides: a hub view that hasn't caught up never makes a second tab.
    fn fill_until(
        &self,
        column_key: &str,
        rec: &PaneRecord,
        dir: &Path,
        deadline: Instant,
    ) -> Result<Option<String>, RexError> {
        if self.inner.holds_other_block(rec, &[], Some(deadline)) {
            return Ok(None);
        }
        let made =
            self.inner
                .new_window_until(&rec.server, &rec.session_id, dir, None, None, deadline)?;
        log::info!(
            "Rex {}: {column_key:?}'s column was empty; opened scratch tab {}",
            rec.server,
            made.window_id
        );
        Ok(Some(made.window_id))
    }

    /// Forget `blocks` of `column_key`, closed: their keys and the sizes asked for them.
    fn unbind_blocks(&self, column_key: &str, blocks: &[&str]) {
        let mut st = self.inner.lock();
        for b in blocks {
            let key = block_key(column_key, b);
            st.blocks.remove(&key);
            st.requested.remove(&key);
        }
    }

    /// [`RexHub::open_column`]'s work, every call waiting until `deadline`.
    fn open_column_until(
        &self,
        req: &RexOpen,
        deadline: Instant,
    ) -> Result<ColumnOpened, RexError> {
        let (key, server) = (req.pane_key.as_str(), req.server.as_str());
        let session_id = match self.find_session_until(key, server, deadline)? {
            Some((_, sid)) => {
                if self.is_restored(key) {
                    if let Err(e) = self.retype_stack(req, &sid, deadline) {
                        // A timed-out write may have landed; never type the stack twice.
                        if matches!(e, RexError::Timeout) {
                            self.inner.lock().stamp(key);
                        }
                        return Err(e);
                    }
                }
                if let Ok(rec) = self.column_record(key) {
                    self.fill_until(key, &rec, &req.dir, deadline)?;
                }
                sid
            }
            None => self.create_column(req, deadline)?,
        };
        {
            let mut st = self.inner.lock();
            // Stamped only once retyped (or timed out), so a failed write retries next open.
            st.stamp(key);
            st.bind_columns(server, &session_id);
        }
        self.column(server, &session_id)
            .ok_or_else(|| api_error("no_view", "the scratch column's session has no view", 0))
    }

    /// A new session for `req`'s column, laid out as its stack, mapped and attached. Returns its id.
    fn create_column(&self, req: &RexOpen, deadline: Instant) -> Result<String, RexError> {
        let (key, server) = (req.pane_key.as_str(), req.server.as_str());
        let (client, _) = self.inner.conn_until(server, deadline)?;
        let previous = self.inner.lock().store.get(key).cloned();
        let window = json!({ "layout": stack_layout(&req.stack, &req.dir) });
        let (_, created) = client.call_seq_until(
            "session.create",
            json!({ "label": req.label, "initial_windows": [window] }),
            deadline,
        )?;
        let session_id = str_field(&created["session_id"], "session.create session_id")?;
        log::info!("Rex {server}: created column session {session_id} ({key})");
        self.inner
            .map_created(key, server, &session_id, previous.as_ref());
        self.inner
            .attach(server, &client, &session_id, Some(deadline))?;
        Ok(session_id)
    }

    /// Type each stack pane's command into its block of restored session `session_id`'s first
    /// window, top to bottom — only while that window holds exactly one block per stack pane;
    /// otherwise the layout is no longer the stack's, and nothing is typed. A stack of bare shells
    /// has nothing to type.
    fn retype_stack(
        &self,
        req: &RexOpen,
        session_id: &str,
        deadline: Instant,
    ) -> Result<(), RexError> {
        let typed =
            |p: &warden_config::StackPane| p.startup.as_deref().is_some_and(|s| !s.is_empty());
        if !req.stack.iter().any(typed) {
            return Ok(());
        }
        let (key, server) = (req.pane_key.as_str(), req.server.as_str());
        let windows = self.windows(server, session_id);
        let blocks = windows
            .first()
            .and_then(|w| w.layout.as_ref())
            .map(Layout::blocks)
            .unwrap_or_default();
        if blocks.len() != req.stack.len() {
            log::info!("Rex {server}: restored column no longer has its stack's shape ({key})");
            return Ok(());
        }
        let (client, _) = self.inner.conn_until(server, deadline)?;
        log::info!("Rex {server}: restored column; retyping its stack ({key})");
        for (block, pane) in blocks.iter().zip(&req.stack) {
            if let Some(s) = pane.startup.as_deref().filter(|s| !s.is_empty()) {
                client.write_input_until(
                    session_id,
                    block,
                    format!("{s}\n").as_bytes(),
                    deadline,
                )?;
            }
        }
        Ok(())
    }

    /// `column_key`'s stored session as `(server, session id)`, if it has one.
    pub(crate) fn column_session(&self, column_key: &str) -> Option<(String, String)> {
        self.column_record(column_key)
            .ok()
            .map(|r| (r.server, r.session_id))
    }

    /// `column_key`'s stored session, or `session_not_found` when it has none.
    fn column_record(&self, column_key: &str) -> Result<PaneRecord, RexError> {
        self.inner
            .lock()
            .store
            .get(column_key)
            .cloned()
            .ok_or_else(|| api_error("session_not_found", "this column has no Rex session", 404))
    }
}
