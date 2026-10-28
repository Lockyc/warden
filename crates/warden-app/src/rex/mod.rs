//! The Rex hub: warden-app's one owner of Rex control connections.
//!
//! One `ControlClient` per server, connected the first time a pane needs that server (or at
//! launch, for the servers config or the store names: [`unclaimed::connect`]) and attached
//! with `session.attach_all` (scope `session`) for the server-wide session list, plus
//! `session.attach` for each session a pane shows. It finds or creates each pane's session —
//! or, for an unclaimed session's pane ([`unclaimed`]), attaches the one it was pointed at —
//! persists pane → session ids ([`store`]), tracks block owners and each attached session's
//! windows and block status ([`windows`]), claims and sizes a pane's block for warden
//! ([`RexHub::claim`], driven by [`size`]), and reports changes to
//! [`RexHub::subscribe`]rs.
//!
//! Threads: the hub's methods block their caller on the network and never hold the state lock
//! across a call. Opening a pane — waiting for another open of the same pane, getting a usable
//! connection (connecting, resolving a host label, reading the session list), then finding or
//! creating its session — shares one deadline, [`OPEN_BUDGET`], and reports `Unreachable` past
//! it, because it runs on the path that opens a tab. Calls on one pane key are serialized, so two
//! opens of a pane never make two sessions. Each connection has one watcher thread that applies its events to the state; when the
//! connection ends, that thread removes it, reports `Disconnected`, and reconnects with the
//! reconnect token (backing off), rereading the session list and attaching the claimed sessions
//! again. A reconnect attempt holds nothing a caller waits on: callers connect alongside it, and
//! whichever connection is installed first wins. Watchers hold the hub weakly: dropping the last
//! `RexHub` closes every connection and ends them.

pub mod reports;
pub mod server;
pub mod size;
pub mod spawn;
pub mod store;
#[cfg(test)]
mod tests;
pub mod unclaimed;
pub mod windows;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use rex_client::{ClientInfo, ControlClient, Endpoint, Event, RexError};
use serde_json::{json, Value};

use server::LOCAL;
use store::{PaneRecord, RexStore};

/// The block creator whose blocks a pane shows.
const TERMINAL_CREATOR: &str = "com.superlogical.terminal";
/// The flavor of the terminal warden creates in a new session.
const TERMINAL_FLAVOR: &str = "com.superlogical.terminal.shell";
/// Reconnect backoff: the first retry is immediate, then this, doubling up to the cap.
const RECONNECT_FIRST: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// The longest opening a pane takes, start to answer, and the longest any caller waits for a
/// usable connection. A tab must open or report its server unreachable within 3 s; the margin
/// covers the work after the deadline.
const OPEN_BUDGET: Duration = Duration::from_millis(2700);
/// The longest session label Rex accepts, in characters; a longer one fails `session.create`.
pub const SESSION_LABEL_MAX: usize = 128;
/// Less budget than this left, and autostart isn't worth starting.
const AUTOSTART_MIN: Duration = Duration::from_millis(250);

/// Where a pane's terminal lives: what the relay connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTarget {
    pub endpoint: Endpoint,
    pub session_id: String,
    pub block_id: String,
}

/// A session on a server, as `session.list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
}

/// What subscribers hear. `server` is the config `server` value the connection serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubEvent {
    /// The server's session list changed; read it with [`RexHub::live_sessions`].
    SessionsChanged {
        server: String,
    },
    /// A block's owner changed; `None` when it no longer has one.
    OwnerChanged {
        session_id: String,
        block_id: String,
        owner: Option<String>,
    },
    /// A watched session's windows changed; read them with [`RexHub::windows`].
    WindowsChanged {
        server: String,
        session_id: String,
    },
    /// A block's status changed; read it with [`RexHub::block_status`].
    StatusChanged {
        block_id: String,
    },
    Disconnected {
        server: String,
    },
    Reconnected {
        server: String,
    },
}

/// Starts the local server, waiting at most the given time.
type Autostart = fn(Duration) -> Result<(), RexError>;

/// Cheap to clone; every clone is the same hub.
#[derive(Clone)]
pub struct RexHub {
    inner: Arc<Inner>,
}

struct Inner {
    client: ClientInfo,
    /// The endpoint `"local"` names.
    local: Endpoint,
    /// Starts the local server when it's unreachable; `None` in tests.
    autostart: Option<Autostart>,
    state: Mutex<State>,
    /// Signalled when a caller's connect attempt for some server ends.
    connected: Condvar,
    subscribers: Mutex<Vec<mpsc::Sender<HubEvent>>>,
    /// Pane keys a call is working on right now ([`Inner::hold_pane`]).
    panes_busy: Mutex<HashSet<String>>,
    /// Signalled when a pane key is released.
    pane_free: Condvar,
}

/// A pane key held by one call; released on drop.
struct PaneHold<'a> {
    inner: &'a Inner,
    key: String,
}

impl Drop for PaneHold<'_> {
    fn drop(&mut self) {
        lock(&self.inner.panes_busy).remove(&self.key);
        self.inner.pane_free.notify_all();
    }
}

#[derive(Default)]
struct State {
    store: RexStore,
    conns: HashMap<String, Conn>,
    /// Servers a caller is connecting to right now, so a second caller waits for that attempt
    /// (within its own budget) instead of opening another connection.
    connecting: HashSet<String>,
    /// Sessions `session.attach`ed per server. Outlives a connection so its replacement attaches
    /// them again.
    attached: HashMap<String, HashSet<String>>,
    /// Pane key → the block it shows, learned when its session was found or created.
    blocks: HashMap<String, String>,
    /// Pane key → the last size warden asked for: by which client, for which block.
    requested: HashMap<String, Requested>,
    /// Pane key → an existing session it was pointed at for this run ([`RexHub::adopt`]): looked
    /// up after the store, never persisted, never claimed, and never replaced by a new session.
    adopted: HashMap<String, PaneRecord>,
    /// A server's session list as last heard on a connection that has since dropped, so its
    /// sessions stay listed until a new connection rereads it.
    heard: HashMap<String, Vec<SessionInfo>>,
    /// (server, session) → the windows of each attached session ([`windows`]).
    windows: HashMap<(String, String), windows::SessionWindows>,
    /// Block → what its program last reported ([`windows`]).
    statuses: HashMap<String, windows::Tracked>,
    next_gen: u64,
}

impl State {
    /// Where `pane_key`'s session lives: its stored mapping, else its adoption.
    fn record(&self, pane_key: &str) -> Option<&PaneRecord> {
        self.store
            .get(pane_key)
            .or_else(|| self.adopted.get(pane_key))
    }

    /// `server`'s session list: its connection's, else the last heard while it's disconnected.
    fn sessions(&self, server: &str) -> Option<&[SessionInfo]> {
        self.conns
            .get(server)
            .map(|c| c.live.as_slice())
            .or_else(|| self.heard.get(server).map(Vec::as_slice))
    }
}

/// A terminal size warden asks a block for: its grid, and a cell's device-pixel size when known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TermSize {
    cols: u16,
    rows: u16,
    cell_px: Option<(u16, u16)>,
}

/// A size warden asked for. The server remembers each client's last request per block, so the
/// same size from the same client to the same block needs no second request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Requested {
    client_id: String,
    block_id: String,
    size: TermSize,
}

/// One open connection and what it has told us. Seqs are this connection's.
struct Conn {
    /// Tells a replacement connection's events from this one's.
    gen: u64,
    client: Arc<ControlClient>,
    endpoint: Endpoint,
    live: Vec<SessionInfo>,
    /// The `seq` of the response `live` came from: session events at or below it are in `live`.
    live_seq: u64,
    /// (session, block) → (seq of the fact, owner).
    owners: HashMap<(String, String), (u64, Option<String>)>,
}

impl Conn {
    /// Record `owner` unless a newer fact is already held; true if the owner changed.
    fn set_owner(&mut self, session: &str, block: &str, seq: u64, owner: Option<String>) -> bool {
        let key = (session.to_owned(), block.to_owned());
        match self.owners.get(&key) {
            Some((held, _)) if *held > seq => false,
            Some((_, prev)) if *prev == owner => {
                self.owners.insert(key, (seq, owner));
                false
            }
            None if owner.is_none() => {
                self.owners.insert(key, (seq, owner));
                false
            }
            _ => {
                self.owners.insert(key, (seq, owner));
                true
            }
        }
    }
}

impl RexHub {
    /// A hub persisting to `store_path`, with `"local"` at this Mac's socket (started on demand).
    pub fn new(store_path: PathBuf, client: ClientInfo) -> RexHub {
        Self::build(
            RexStore::load(store_path),
            client,
            Endpoint::local(),
            Some(server::autostart_local_within),
        )
    }

    /// [`RexHub::new`] with nowhere to persist: panes reattach only within this run.
    pub fn in_memory(client: ClientInfo) -> RexHub {
        Self::build(
            RexStore::default(),
            client,
            Endpoint::local(),
            Some(server::autostart_local_within),
        )
    }

    /// A hub whose `"local"` is `local` (a fake server), with no autostart.
    #[cfg(test)]
    pub(crate) fn for_test(store_path: PathBuf, client: ClientInfo, local: Endpoint) -> RexHub {
        Self::build(RexStore::load(store_path), client, local, None)
    }

    /// [`RexHub::for_test`] whose unreachable `"local"` runs `autostart`.
    #[cfg(test)]
    pub(crate) fn for_test_autostart(
        store_path: PathBuf,
        client: ClientInfo,
        local: Endpoint,
        autostart: Autostart,
    ) -> RexHub {
        Self::build(RexStore::load(store_path), client, local, Some(autostart))
    }

    fn build(
        store: RexStore,
        client: ClientInfo,
        local: Endpoint,
        autostart: Option<Autostart>,
    ) -> RexHub {
        RexHub {
            inner: Arc::new(Inner {
                client,
                local,
                autostart,
                state: Mutex::new(State {
                    store,
                    ..State::default()
                }),
                connected: Condvar::new(),
                subscribers: Mutex::default(),
                panes_busy: Mutex::default(),
                pane_free: Condvar::new(),
            }),
        }
    }

    /// The session `pane_key` shows on `server`: its mapped session if that still exists there,
    /// else a new one with one terminal in `dir`, typing `startup` and a newline. A new mapping
    /// is persisted. The session is attached on the hub's connection either way.
    ///
    /// The whole call, waiting included, ends by [`OPEN_BUDGET`]: past it the answer is
    /// `Unreachable`. A second open of the same pane waits for the first and then reattaches what
    /// it found or made.
    pub fn ensure_session(
        &self,
        pane_key: &str,
        server: &str,
        dir: &Path,
        startup: Option<&str>,
        label: &str,
    ) -> Result<PaneTarget, RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        let _held = self.inner.hold_pane(pane_key, Some(deadline))?;
        match self
            .inner
            .ensure_until(pane_key, server, dir, startup, label, deadline)
        {
            Err(RexError::Timeout) => Err(RexError::Unreachable),
            Err(_) if Instant::now() >= deadline => Err(RexError::Unreachable),
            r => r,
        }
    }

    /// Let `pane_key` go of its session if its terminal has finished ([`RexHub::block_exited`]),
    /// as one step no open of that pane can come between: the session is destroyed when that
    /// block was its last, else only the mapping goes, so blocks made elsewhere keep running and
    /// the session lists as unclaimed. Returns whether the pane let go.
    pub fn destroy_if_exited(&self, pane_key: &str) -> bool {
        let Ok(_held) = self.inner.hold_pane(pane_key, None) else {
            return false;
        };
        if !self.block_exited(pane_key) {
            return false;
        }
        if self.inner.session_outlives_pane(pane_key) {
            self.inner.forget(pane_key);
        } else if let Err(e) = self.inner.destroy(pane_key) {
            eprintln!("warden: couldn't end Rex session for {pane_key:?}: {e}");
        }
        true
    }

    /// Whether `pane_key` is mapped to a session its server's session list holds — or, for an
    /// unclaimed session's row not yet opened (so not yet adopted), whether its session is in any
    /// server's list. The list is the live one, or the last heard while the server is
    /// disconnected: a session isn't gone because warden's control link is. False when it's
    /// unmapped, or the server was never heard from.
    pub fn session_alive(&self, pane_key: &str) -> bool {
        let st = self.inner.lock();
        let listed = |list: &[SessionInfo], id: &str| list.iter().any(|s| s.id == id);
        match st.record(pane_key) {
            Some(rec) => st
                .sessions(&rec.server)
                .is_some_and(|l| listed(l, &rec.session_id)),
            None => unclaimed::session_of(pane_key).is_some_and(|id| {
                st.conns.values().any(|c| listed(&c.live, id))
                    || st.heard.values().any(|l| listed(l, id))
            }),
        }
    }

    /// End `pane_key`'s session and forget its mapping. The mapping goes even if the server
    /// can't be told, so the pane never reattaches a session it was asked to end; a session that
    /// survives that shows as unclaimed. An unmapped pane is a no-op.
    pub fn destroy(&self, pane_key: &str) -> Result<(), RexError> {
        let _held = self.inner.hold_pane(pane_key, None)?;
        self.inner.destroy(pane_key)
    }
}

impl Inner {
    /// [`RexHub::ensure_session`]'s work, every call waiting until `deadline`.
    fn ensure_until(
        self: &Arc<Self>,
        pane_key: &str,
        server: &str,
        dir: &Path,
        startup: Option<&str>,
        label: &str,
        deadline: Instant,
    ) -> Result<PaneTarget, RexError> {
        let inner = self;
        let (client, endpoint) = inner.conn_until(server, deadline)?;
        let (previous, adopted) = {
            let st = inner.lock();
            match st.store.get(pane_key) {
                Some(r) => (Some(r.clone()), false),
                None => (st.adopted.get(pane_key).cloned(), true),
            }
        };
        let adopted = adopted && previous.is_some();
        let mut no_terminal = false;
        if let Some(rec) = previous.as_ref().filter(|r| r.server == server) {
            match inner.attach(server, &client, &rec.session_id, Some(deadline)) {
                Ok(view) => {
                    if let Some(block_id) = terminal_block(&view) {
                        inner
                            .lock()
                            .blocks
                            .insert(pane_key.to_owned(), block_id.clone());
                        return Ok(PaneTarget {
                            endpoint,
                            session_id: rec.session_id.clone(),
                            block_id,
                        });
                    }
                    // No terminal left in it: leave it for other clients, make a new one.
                    no_terminal = true;
                }
                Err(RexError::Api { code, .. }) if code == "session_not_found" => {}
                Err(e) => return Err(e),
            }
        }
        // An adopted pane shows the session it was pointed at, or nothing.
        if adopted {
            return Err(if no_terminal {
                api_error("no_terminal", "this Rex session has no terminal to show", 0)
            } else {
                api_error("session_not_found", "this Rex session has ended", 404)
            });
        }

        let options = terminal_options(dir, startup);
        let (_, created) = client.call_seq_until(
            "session.create",
            json!({
                "label": label,
                "initial_windows": [
                    {"layout": {"block": {"flavor": TERMINAL_FLAVOR, "options": options}}}
                ],
            }),
            deadline,
        )?;
        let session_id = str_field(&created["session_id"], "session.create session_id")?;
        let block_id = str_field(
            &created["initial_windows"][0]["block_ids"][0],
            "session.create block id",
        )?;
        {
            let mut st = inner.lock();
            if let Some(prev) = &previous {
                if let Some(a) = st.attached.get_mut(&prev.server) {
                    a.remove(&prev.session_id);
                }
                st.windows
                    .remove(&(prev.server.clone(), prev.session_id.clone()));
            }
            st.store.set(
                pane_key,
                Some(PaneRecord {
                    server: server.to_owned(),
                    session_id: session_id.clone(),
                }),
            );
            st.blocks.insert(pane_key.to_owned(), block_id.clone());
        }
        inner.attach(server, &client, &session_id, Some(deadline))?;
        Ok(PaneTarget {
            endpoint,
            session_id,
            block_id,
        })
    }

    /// [`RexHub::destroy`]'s work, for a caller already holding `pane_key`.
    fn destroy(self: &Arc<Self>, pane_key: &str) -> Result<(), RexError> {
        let Some(rec) = self.forget(pane_key) else {
            return Ok(());
        };
        let (client, _) = self.conn(&rec.server)?;
        match client.call("session.destroy", json!({ "session_id": rec.session_id })) {
            Ok(_) => Ok(()),
            Err(RexError::Api { code, .. }) if code == "session_not_found" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Drop everything warden holds for `pane_key` — its mapping, adoption, block and size — and
    /// return the session it was mapped to. The session itself is untouched.
    fn forget(&self, pane_key: &str) -> Option<PaneRecord> {
        let mut st = self.lock();
        let rec = st.record(pane_key).cloned()?;
        st.store.set(pane_key, None);
        st.adopted.remove(pane_key);
        st.blocks.remove(pane_key);
        st.requested.remove(pane_key);
        if let Some(a) = st.attached.get_mut(&rec.server) {
            a.remove(&rec.session_id);
        }
        st.windows
            .remove(&(rec.server.clone(), rec.session_id.clone()));
        Some(rec)
    }

    /// Whether `pane_key`'s session holds a block besides the pane's own — placed or detached,
    /// made by anyone — per `session.list_blocks`. True when it can't be told, so an uncertain
    /// answer never destroys another client's work; false once the session is gone.
    fn session_outlives_pane(self: &Arc<Self>, pane_key: &str) -> bool {
        let (rec, block) = {
            let st = self.lock();
            match (st.record(pane_key), st.blocks.get(pane_key)) {
                (Some(r), Some(b)) => (r.clone(), b.clone()),
                _ => return true,
            }
        };
        let Ok((client, _)) = self.conn(&rec.server) else {
            return true;
        };
        let args = json!({ "session_id": rec.session_id });
        match client.call("session.list_blocks", args) {
            Ok(v) => v["blocks"].as_array().is_none_or(|blocks| {
                blocks
                    .iter()
                    .any(|b| b["block_id"].as_str() != Some(block.as_str()))
            }),
            Err(RexError::Api { code, .. }) => code != "session_not_found",
            Err(_) => true,
        }
    }

    /// [`RexHub::claim`] (`take`) and [`RexHub::resize_if_owner`] (not `take`), within
    /// [`OPEN_BUDGET`]. Uses only the server's current connection, never opens one: a pane with
    /// no connection has no session showing to size, and its reconnect restores ownership.
    fn size(&self, pane_key: &str, size: TermSize, take: bool) -> Result<bool, RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        let _held = self.hold_pane(pane_key, Some(deadline))?;
        let (server, client, session_id, block_id, owns, asked) = {
            let st = self.lock();
            let (Some(rec), Some(block_id)) = (st.record(pane_key), st.blocks.get(pane_key)) else {
                return Ok(false);
            };
            let Some(conn) = st.conns.get(&rec.server) else {
                return Ok(false);
            };
            let me = conn.client.id();
            let key = (rec.session_id.clone(), block_id.clone());
            let owns = conn.owners.get(&key).and_then(|(_, o)| o.as_deref()) == Some(me);
            let asked = st
                .requested
                .get(pane_key)
                .is_some_and(|r| r.client_id == me && r.block_id == *block_id && r.size == size);
            (
                rec.server.clone(),
                conn.client.clone(),
                rec.session_id.clone(),
                block_id.clone(),
                owns,
                asked,
            )
        };
        if !owns {
            if !take {
                return Ok(false);
            }
            let (seq, result) = client.call_seq_until(
                "session.own_block",
                json!({ "session_id": session_id, "block_ids": [block_id], "mode": "take" }),
                deadline,
            )?;
            let blocks = result["blocks"].as_array().into_iter().flatten();
            self.record_owners(&server, &client, &session_id, seq, blocks);
        } else if asked {
            return Ok(false);
        }
        let mut args = json!({ "columns": size.cols, "rows": size.rows });
        if let Some((w, h)) = size.cell_px {
            args["cell_width_px"] = json!(w);
            args["cell_height_px"] = json!(h);
        }
        client.call_seq_until(
            &format!("{TERMINAL_CREATOR}.resize"),
            json!({ "session_id": session_id, "block_id": block_id, "args": args }),
            deadline,
        )?;
        self.lock().requested.insert(
            pane_key.to_owned(),
            Requested {
                client_id: client.id().to_owned(),
                block_id,
                size,
            },
        );
        Ok(true)
    }
}

impl RexHub {
    /// Whether `pane_key`'s terminal has finished: `terminal.process` reports an exit, or its
    /// session or block is gone. False when it can't be told (unmapped, or unreachable), so an
    /// uncertain answer never ends a session.
    pub fn block_exited(&self, pane_key: &str) -> bool {
        let (rec, block) = {
            let st = self.inner.lock();
            (
                st.record(pane_key).cloned(),
                st.blocks.get(pane_key).cloned(),
            )
        };
        let (Some(rec), Some(block)) = (rec, block) else {
            return false;
        };
        let Ok((client, _)) = self.inner.conn(&rec.server) else {
            return false;
        };
        match client.block_call(&rec.session_id, &block, "process", Value::Null) {
            Ok(v) => !v["last_exit"].is_null(),
            Err(RexError::Api { code, .. }) => code == "session_not_found" || code == "not_found",
            Err(_) => false,
        }
    }

    /// Make warden the owner of `pane_key`'s block at `cols`×`rows` (and a cell of `cell_px`
    /// device pixels, when known): `session.own_block` `take` unless this connection's client
    /// already owns it, then `terminal.resize` unless this client already asked for this size.
    /// Owning it at this size costs no request, so a caller may claim on every keystroke. Returns
    /// whether anything was sent; `Ok(false)` for an unmapped pane or an unconnected server.
    ///
    /// The owner compared against is the one last heard on the *current* connection, as this
    /// connection's client: a connection that replaced the one warden claimed on (a new client id)
    /// reads as not owning, so the next claim takes the block again.
    pub fn claim(
        &self,
        pane_key: &str,
        cols: u16,
        rows: u16,
        cell_px: Option<(u16, u16)>,
    ) -> Result<bool, RexError> {
        let size = TermSize {
            cols,
            rows,
            cell_px,
        };
        self.inner.size(pane_key, size, true)
    }

    /// Send `pane_key`'s new size, only while warden's client owns its block and hasn't already
    /// asked for this size. Returns whether it sent one.
    pub fn resize_if_owner(
        &self,
        pane_key: &str,
        cols: u16,
        rows: u16,
        cell_px: Option<(u16, u16)>,
    ) -> Result<bool, RexError> {
        let size = TermSize {
            cols,
            rows,
            cell_px,
        };
        self.inner.size(pane_key, size, false)
    }

    /// The sessions on `server`, as last heard — kept while its connection is down, since its
    /// sessions outlive the connection; empty when it never connected.
    pub fn live_sessions(&self, server: &str) -> Vec<SessionInfo> {
        self.inner
            .lock()
            .sessions(server)
            .map(<[SessionInfo]>::to_vec)
            .unwrap_or_default()
    }

    /// Every server the hub has heard from — connected, or dropped with its last list kept —
    /// sorted.
    pub fn servers(&self) -> Vec<String> {
        let st = self.inner.lock();
        let mut servers: Vec<String> = st.conns.keys().chain(st.heard.keys()).cloned().collect();
        servers.sort();
        servers.dedup();
        servers
    }

    /// Every server a stored pane mapping names.
    pub fn store_servers(&self) -> Vec<String> {
        self.inner.lock().store.servers()
    }

    /// The sessions `pane_keys` map to in the store, on any server. An adopted pane claims
    /// nothing: its session is still the unclaimed one it was opened from.
    pub fn claimed_session_ids<'k>(
        &self,
        pane_keys: impl IntoIterator<Item = &'k str>,
    ) -> HashSet<String> {
        let st = self.inner.lock();
        pane_keys
            .into_iter()
            .filter_map(|k| st.store.get(k))
            .map(|r| r.session_id.clone())
            .collect()
    }

    /// Point `pane_key` at `server`'s existing session `session_id`, for this run only: its
    /// [`RexHub::ensure_session`] attaches that session, or fails, and never creates one.
    /// A stored mapping for `pane_key` still wins.
    pub fn adopt(&self, pane_key: &str, server: &str, session_id: &str) {
        self.inner.lock().adopted.insert(
            pane_key.to_owned(),
            PaneRecord {
                server: server.to_owned(),
                session_id: session_id.to_owned(),
            },
        );
    }

    /// Connect to `server` now, within [`OPEN_BUDGET`], if it isn't connected — without starting
    /// the local server: only opening a Rex tab does that, so a Rex the user quit stays quit.
    pub fn connect(&self, server: &str) -> Result<(), RexError> {
        let deadline = Instant::now() + OPEN_BUDGET;
        self.inner.conn_with(server, deadline, false).map(|_| ())
    }

    /// A receiver for every [`HubEvent`] from now on.
    pub fn subscribe(&self) -> mpsc::Receiver<HubEvent> {
        let (tx, rx) = mpsc::channel();
        lock(&self.inner.subscribers).push(tx);
        rx
    }

    /// The client that owns `block_id`, as last heard on a connection attached to its session.
    #[cfg(test)]
    pub fn owner(&self, session_id: &str, block_id: &str) -> Option<String> {
        let key = (session_id.to_owned(), block_id.to_owned());
        self.inner
            .lock()
            .conns
            .values()
            .find_map(|c| c.owners.get(&key))
            .and_then(|(_, o)| o.clone())
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn emit(&self, events: impl IntoIterator<Item = HubEvent>) {
        let mut subs = lock(&self.subscribers);
        for ev in events {
            subs.retain(|tx| tx.send(ev.clone()).is_ok());
        }
    }

    fn current(&self, server: &str) -> Option<(Arc<ControlClient>, Endpoint)> {
        self.lock()
            .conns
            .get(server)
            .map(|c| (c.client.clone(), c.endpoint.clone()))
    }

    /// `server`'s connection, opening it if there is none, within [`OPEN_BUDGET`] — never
    /// starting Rex: only a tab open does that ([`Inner::conn_until`]). Ending or checking a
    /// session on a server that isn't running has nothing to end or tell.
    fn conn(self: &Arc<Self>, server: &str) -> Result<(Arc<ControlClient>, Endpoint), RexError> {
        self.conn_with(server, Instant::now() + OPEN_BUDGET, false)
    }

    /// Hold `pane_key` until the guard drops, waiting while another call holds it — until
    /// `deadline` when one is set (then `Unreachable`), else for as long as it takes: every
    /// holder is itself bounded.
    fn hold_pane(
        &self,
        pane_key: &str,
        deadline: Option<Instant>,
    ) -> Result<PaneHold<'_>, RexError> {
        let mut busy = lock(&self.panes_busy);
        while busy.contains(pane_key) {
            busy = match deadline {
                None => self.pane_free.wait(busy).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(RexError::Unreachable);
                    }
                    self.pane_free
                        .wait_timeout(busy, left)
                        .map(|(g, _)| g)
                        .unwrap_or_else(|e| e.into_inner().0)
                }
            };
        }
        busy.insert(pane_key.to_owned());
        Ok(PaneHold {
            inner: self,
            key: pane_key.to_owned(),
        })
    }

    /// [`Inner::conn_with`], starting the local server if it must.
    fn conn_until(
        self: &Arc<Self>,
        server: &str,
        deadline: Instant,
    ) -> Result<(Arc<ControlClient>, Endpoint), RexError> {
        self.conn_with(server, deadline, true)
    }

    /// `server`'s connection, or `Unreachable` if none is usable by `deadline`. While another
    /// caller is connecting to `server` this waits for that attempt rather than opening a second
    /// connection, then tries itself if it failed. `start`: whether an unreachable local server
    /// may be started for it.
    fn conn_with(
        self: &Arc<Self>,
        server: &str,
        deadline: Instant,
        start: bool,
    ) -> Result<(Arc<ControlClient>, Endpoint), RexError> {
        {
            let mut st = self.lock();
            loop {
                if let Some(c) = st.conns.get(server) {
                    return Ok((c.client.clone(), c.endpoint.clone()));
                }
                if !st.connecting.contains(server) {
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(RexError::Unreachable);
                }
                st = self
                    .connected
                    .wait_timeout(st, left)
                    .map(|(g, _)| g)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
            st.connecting.insert(server.to_owned());
        }
        let opened = self.open(server, deadline, start);
        self.lock().connecting.remove(server);
        self.connected.notify_all();
        match opened {
            Ok(c) => Ok(c),
            Err(RexError::Timeout) => Err(RexError::Unreachable),
            Err(_) if Instant::now() >= deadline => Err(RexError::Unreachable),
            Err(e) => Err(e),
        }
    }

    /// Open and install `server`'s connection by `deadline`. A label resolves through the local
    /// server's host list; an unreachable local server is started once and retried, when `start`.
    fn open(
        self: &Arc<Self>,
        server: &str,
        deadline: Instant,
        start: bool,
    ) -> Result<(Arc<ControlClient>, Endpoint), RexError> {
        let endpoint = if server == LOCAL {
            self.local.clone()
        } else {
            let (local, _) = self.conn_with(LOCAL, deadline, start)?;
            server::resolve_until(server, &local, deadline)?
        };
        let connect = || ControlClient::connect_until(&endpoint, &self.client, deadline);
        let client = match (connect(), self.autostart.filter(|_| start)) {
            (Err(RexError::Unreachable), Some(start)) if server == LOCAL => {
                // Autostart gets what's left of this caller's budget; with too little left to
                // start and still connect, it isn't tried.
                let left = deadline.saturating_duration_since(Instant::now());
                if left < AUTOSTART_MIN {
                    return Err(RexError::Unreachable);
                }
                start(left)?;
                connect()?
            }
            (r, _) => r?,
        };
        let (client, _) = self.install(server, endpoint.clone(), client, Some(deadline))?;
        Ok((client, endpoint))
    }

    /// Read the session list on a fresh connection (`session.attach_all`, or `session.list` when
    /// it resumed a client that still has it) and make it `server`'s connection — unless one is
    /// already installed, which stays while this one closes, so a late attempt never replaces a
    /// newer connection. Then attach the sessions this server had attached and start the
    /// watcher. Calls wait until `deadline` when one is set. Returns the server's connection and
    /// whether it is this one.
    fn install(
        self: &Arc<Self>,
        server: &str,
        endpoint: Endpoint,
        client: ControlClient,
        deadline: Option<Instant>,
    ) -> Result<(Arc<ControlClient>, bool), RexError> {
        let events = client.events();
        let (seq, list) = if client.resumed() {
            call_seq(&client, "session.list", json!({}), deadline)?
        } else {
            let scope = json!({ "event_scope": "session" });
            call_seq(&client, "session.attach_all", scope, deadline)?
        };
        let client = Arc::new(client);
        let token = client.reconnect_token().map(str::to_owned);
        let gen = {
            let mut st = self.lock();
            if let Some(c) = st.conns.get(server) {
                return Ok((c.client.clone(), false));
            }
            st.heard.remove(server);
            st.next_gen += 1;
            let gen = st.next_gen;
            st.conns.insert(
                server.to_owned(),
                Conn {
                    gen,
                    client: client.clone(),
                    endpoint: endpoint.clone(),
                    live: parse_sessions(&list["sessions"]),
                    live_seq: seq,
                    owners: HashMap::new(),
                },
            );
            gen
        };
        let attached: Vec<String> = self
            .lock()
            .attached
            .get(server)
            .map(|a| a.iter().cloned().collect())
            .unwrap_or_default();
        for id in attached {
            if let Err(RexError::Api { code, .. }) = self.attach(server, &client, &id, deadline) {
                if code == "session_not_found" {
                    if let Some(a) = self.lock().attached.get_mut(server) {
                        a.remove(&id);
                    }
                }
            }
        }
        let weak = Arc::downgrade(self);
        let server = server.to_owned();
        thread::spawn(move || watch(weak, server, gen, events, endpoint, token));
        Ok((client, true))
    }

    /// `session.attach` `session_id` on `client`, recording its blocks' owners and its windows
    /// and pulling its blocks' status ([`windows`]); returns the view.
    fn attach(
        self: &Arc<Self>,
        server: &str,
        client: &Arc<ControlClient>,
        session_id: &str,
        deadline: Option<Instant>,
    ) -> Result<Value, RexError> {
        let args = json!({ "session_id": session_id });
        self.watch_windows(server, session_id);
        let (seq, result) = match call_seq(client, "session.attach", args, deadline) {
            Ok(r) => r,
            Err(e) => {
                self.unwatch_unviewed(server, session_id);
                return Err(e);
            }
        };
        let view = result["view"].clone();
        self.lock()
            .attached
            .entry(server.to_owned())
            .or_default()
            .insert(session_id.to_owned());
        self.record_owners(server, client, session_id, seq, view_blocks(&view, None));
        self.learn_view(server, client, session_id, seq, &view, true);
        Ok(view)
    }

    /// Record the owners `blocks` (`{block_id, owner}` objects, as a view or `own_block` lists
    /// them) report as of `seq`, if `client` is still `server`'s connection, and report changes.
    fn record_owners<'v>(
        &self,
        server: &str,
        client: &Arc<ControlClient>,
        session_id: &str,
        seq: u64,
        blocks: impl Iterator<Item = &'v Value>,
    ) {
        let mut changed = Vec::new();
        {
            let mut st = self.lock();
            if let Some(conn) = st
                .conns
                .get_mut(server)
                .filter(|c| Arc::ptr_eq(&c.client, client))
            {
                for block in blocks {
                    let Some(block_id) = block["block_id"].as_str() else {
                        continue;
                    };
                    let owner = block["owner"].as_str().map(str::to_owned);
                    if conn.set_owner(session_id, block_id, seq, owner.clone()) {
                        changed.push(HubEvent::OwnerChanged {
                            session_id: session_id.to_owned(),
                            block_id: block_id.to_owned(),
                            owner,
                        });
                    }
                }
            }
        }
        self.emit(changed);
    }

    /// Apply one event from connection `gen` of `server`; a replaced connection's are ignored.
    fn apply(self: &Arc<Self>, server: &str, gen: u64, ev: Event) {
        let mut out = Vec::new();
        let refresh = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(conn) = st.conns.get_mut(server).filter(|c| c.gen == gen) else {
                return;
            };
            let refresh = windows::apply_event(
                &mut st.windows,
                &mut st.statuses,
                server,
                gen,
                &ev,
                &mut out,
            )
            .map(|session| (conn.client.clone(), session));
            let sessions_changed = || HubEvent::SessionsChanged {
                server: server.to_owned(),
            };
            match ev.kind.as_str() {
                "session_created" if ev.seq > conn.live_seq => {
                    if let Some(s) = parse_session(&ev.payload["session"]) {
                        match conn.live.iter_mut().find(|l| l.id == s.id) {
                            Some(l) => l.label = s.label,
                            None => conn.live.push(s),
                        }
                        out.push(sessions_changed());
                    }
                }
                "session_destroyed" if ev.seq > conn.live_seq => {
                    if let Some(id) = ev.session_id {
                        conn.live.retain(|l| l.id != id);
                        conn.owners.retain(|(s, _), _| *s != id);
                        if let Some(a) = st.attached.get_mut(server) {
                            a.remove(&id);
                        }
                        st.adopted
                            .retain(|_, r| !(r.server == server && r.session_id == id));
                        if st
                            .windows
                            .remove(&(server.to_owned(), id.clone()))
                            .is_some()
                        {
                            out.push(HubEvent::WindowsChanged {
                                server: server.to_owned(),
                                session_id: id,
                            });
                        }
                        out.push(sessions_changed());
                    }
                }
                "block_owner_changed" => {
                    let block = ev.payload["block_id"].as_str();
                    if let (Some(session), Some(block)) = (ev.session_id, block) {
                        let owner = ev.payload["owner"].as_str().map(str::to_owned);
                        if conn.set_owner(&session, block, ev.seq, owner.clone()) {
                            out.push(HubEvent::OwnerChanged {
                                session_id: session,
                                block_id: block.to_owned(),
                                owner,
                            });
                        }
                    }
                }
                _ => {}
            }
            refresh
        };
        self.emit(out);
        if let Some((client, session)) = refresh {
            self.spawn_refresh(server, &client, &session);
        }
    }

    /// Reopen `server`'s connection after a drop, resuming its client when `token` is still
    /// good. `Ok(false)` when a caller connected it first. Holds nothing a caller waits on: the
    /// attempt runs alongside any caller's, and `install` keeps whichever lands first.
    fn reconnect(
        self: &Arc<Self>,
        server: &str,
        endpoint: &Endpoint,
        token: Option<&str>,
    ) -> Result<bool, RexError> {
        if self.current(server).is_some() {
            return Ok(false);
        }
        let client = match token {
            Some(t) => ControlClient::resume(endpoint, &self.client, t)?,
            None => ControlClient::connect(endpoint, &self.client)?,
        };
        let (_, installed) = self.install(server, endpoint.clone(), client, None)?;
        Ok(installed)
    }
}

/// A connection's watcher: apply its events until it ends, then drop it and reconnect.
fn watch(
    hub: Weak<Inner>,
    server: String,
    gen: u64,
    events: mpsc::Receiver<Event>,
    endpoint: Endpoint,
    token: Option<String>,
) {
    for ev in events {
        let Some(inner) = hub.upgrade() else {
            return;
        };
        inner.apply(&server, gen, ev);
    }
    {
        let Some(inner) = hub.upgrade() else {
            return;
        };
        let removed = {
            let mut st = inner.lock();
            let ours = st.conns.get(&server).is_some_and(|c| c.gen == gen);
            if ours {
                if let Some(c) = st.conns.remove(&server) {
                    st.heard.insert(server.clone(), c.live);
                }
            }
            ours
        };
        if !removed {
            return;
        }
        inner.emit([HubEvent::Disconnected {
            server: server.clone(),
        }]);
    }
    let mut delay = Duration::ZERO;
    loop {
        thread::sleep(delay);
        let Some(inner) = hub.upgrade() else {
            return;
        };
        match inner.reconnect(&server, &endpoint, token.as_deref()) {
            Ok(true) => {
                inner.emit([
                    HubEvent::Reconnected {
                        server: server.clone(),
                    },
                    HubEvent::SessionsChanged { server },
                ]);
                return;
            }
            Ok(false) => return,
            Err(_) => {}
        }
        delay = (delay * 2).clamp(RECONNECT_FIRST, RECONNECT_MAX);
    }
}

/// `call_seq`, waiting until `deadline` when one is set, else rex-client's default.
fn call_seq(
    client: &ControlClient,
    method: &str,
    payload: Value,
    deadline: Option<Instant>,
) -> Result<(u64, Value), RexError> {
    match deadline {
        Some(d) => client.call_seq_until(method, payload, d),
        None => client.call_seq(method, payload),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn api_error(code: &str, message: &str, status: u16) -> RexError {
    RexError::Api {
        code: code.to_owned(),
        message: message.to_owned(),
        status,
    }
}

fn str_field(v: &Value, what: &str) -> Result<String, RexError> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| RexError::Protocol(format!("{what} missing")))
}

/// A new terminal's options: started in `dir`, typing `startup` and a newline.
fn terminal_options(dir: &Path, startup: Option<&str>) -> Value {
    let mut options = json!({ "cwd": dir.to_string_lossy() });
    if let Some(s) = startup.filter(|s| !s.is_empty()) {
        options["initial_input"] = json!(format!("{s}\n"));
    }
    options
}

fn parse_session(v: &Value) -> Option<SessionInfo> {
    Some(SessionInfo {
        id: v["session_id"].as_str()?.to_owned(),
        label: v["label"].as_str().unwrap_or_default().to_owned(),
    })
}

fn parse_sessions(v: &Value) -> Vec<SessionInfo> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_session)
        .collect()
}

/// The blocks in a `session.view`: every window's, or only the window at `window` (an index).
fn view_blocks(view: &Value, window: Option<usize>) -> impl Iterator<Item = &Value> {
    view["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(move |(i, _)| window.is_none_or(|w| w == *i))
        .flat_map(|(_, w)| w["layers"].as_array().into_iter().flatten())
        .flat_map(|l| l["blocks"].as_array().into_iter().flatten())
}

/// The block a pane shows: the first terminal in the session's first window.
fn terminal_block(view: &Value) -> Option<String> {
    view_blocks(view, Some(0))
        .find(|b| b["creator_name"].as_str() == Some(TERMINAL_CREATOR))
        .and_then(|b| b["block_id"].as_str())
        .map(str::to_owned)
}
