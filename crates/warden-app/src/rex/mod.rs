//! The Rex hub: warden-app's one owner of Rex control connections.
//!
//! One `ControlClient` per server, connected the first time a pane needs that server and attached
//! with `session.attach_all` (scope `session`) for the server-wide session list, plus
//! `session.attach` for each session a pane shows. It finds or creates each pane's session,
//! persists pane → session ids ([`store`]), tracks block owners, and reports changes to
//! [`RexHub::subscribe`]rs.
//!
//! Threads: the hub's methods block their caller on the network (each call is bounded by
//! rex-client's timeouts) and never hold the state lock across a call. Each connection has one
//! watcher thread that applies its events to the state; when the connection ends, that thread
//! removes it, reports `Disconnected`, and reconnects with the reconnect token (backing off),
//! rereading the session list and attaching the claimed sessions again. Watchers hold the hub
//! weakly: dropping the last `RexHub` closes every connection and ends them.

pub mod server;
pub mod store;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::Duration;

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
    Disconnected {
        server: String,
    },
    Reconnected {
        server: String,
    },
}

type Autostart = fn() -> Result<(), RexError>;

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
    /// One gate per server, so two callers needing a connection open it once.
    gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    subscribers: Mutex<Vec<mpsc::Sender<HubEvent>>>,
}

#[derive(Default)]
struct State {
    store: RexStore,
    conns: HashMap<String, Conn>,
    /// Sessions `session.attach`ed per server. Outlives a connection so its replacement attaches
    /// them again.
    attached: HashMap<String, HashSet<String>>,
    /// Pane key → the block it shows, learned when its session was found or created.
    blocks: HashMap<String, String>,
    next_gen: u64,
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
            Some(server::autostart_local),
        )
    }

    /// [`RexHub::new`] with nowhere to persist: panes reattach only within this run.
    pub fn in_memory(client: ClientInfo) -> RexHub {
        Self::build(
            RexStore::default(),
            client,
            Endpoint::local(),
            Some(server::autostart_local),
        )
    }

    /// A hub whose `"local"` is `local` (a fake server), with no autostart.
    #[cfg(test)]
    pub(crate) fn for_test(store_path: PathBuf, client: ClientInfo, local: Endpoint) -> RexHub {
        Self::build(RexStore::load(store_path), client, local, None)
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
                gates: Mutex::default(),
                subscribers: Mutex::default(),
            }),
        }
    }

    /// The session `pane_key` shows on `server`: its mapped session if that still exists there,
    /// else a new one with one terminal in `dir`, typing `startup` and a newline. A new mapping
    /// is persisted. The session is attached on the hub's connection either way.
    pub fn ensure_session(
        &self,
        pane_key: &str,
        server: &str,
        dir: &Path,
        startup: Option<&str>,
        label: &str,
    ) -> Result<PaneTarget, RexError> {
        let inner = &self.inner;
        let (client, endpoint) = inner.conn(server)?;
        let previous = inner.lock().store.get(pane_key).cloned();
        if let Some(rec) = previous.as_ref().filter(|r| r.server == server) {
            match inner.attach(server, &client, &rec.session_id) {
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
                }
                Err(RexError::Api { code, .. }) if code == "session_not_found" => {}
                Err(e) => return Err(e),
            }
        }

        let mut options = json!({ "cwd": dir.to_string_lossy() });
        if let Some(s) = startup.filter(|s| !s.is_empty()) {
            options["initial_input"] = json!(format!("{s}\n"));
        }
        let created = client.call(
            "session.create",
            json!({
                "label": label,
                "initial_windows": [
                    {"layout": {"block": {"flavor": TERMINAL_FLAVOR, "options": options}}}
                ],
            }),
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
        inner.attach(server, &client, &session_id)?;
        Ok(PaneTarget {
            endpoint,
            session_id,
            block_id,
        })
    }

    /// End `pane_key`'s session and forget its mapping. The mapping goes even if the server
    /// can't be told, so the pane never reattaches a session it was asked to end; a session that
    /// survives that shows as unclaimed. An unmapped pane is a no-op.
    pub fn destroy(&self, pane_key: &str) -> Result<(), RexError> {
        let inner = &self.inner;
        let rec = {
            let mut st = inner.lock();
            let rec = st.store.get(pane_key).cloned();
            if let Some(r) = &rec {
                st.store.set(pane_key, None);
                st.blocks.remove(pane_key);
                if let Some(a) = st.attached.get_mut(&r.server) {
                    a.remove(&r.session_id);
                }
            }
            rec
        };
        let Some(rec) = rec else {
            return Ok(());
        };
        let (client, _) = inner.conn(&rec.server)?;
        match client.call("session.destroy", json!({ "session_id": rec.session_id })) {
            Ok(_) => Ok(()),
            Err(RexError::Api { code, .. }) if code == "session_not_found" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether `pane_key`'s terminal has finished: `terminal.process` reports an exit, or its
    /// session or block is gone. False when it can't be told (unmapped, or unreachable), so an
    /// uncertain answer never ends a session.
    pub fn block_exited(&self, pane_key: &str) -> bool {
        let (rec, block) = {
            let st = self.inner.lock();
            (
                st.store.get(pane_key).cloned(),
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

    /// The sessions on `server`, as last heard; empty when it isn't connected.
    pub fn live_sessions(&self, server: &str) -> Vec<SessionInfo> {
        self.inner
            .lock()
            .conns
            .get(server)
            .map(|c| c.live.clone())
            .unwrap_or_default()
    }

    /// Every session some pane is mapped to, on any server.
    pub fn claimed_session_ids(&self) -> HashSet<String> {
        self.inner
            .lock()
            .store
            .records()
            .map(|(_, r)| r.session_id.clone())
            .collect()
    }

    /// A receiver for every [`HubEvent`] from now on.
    pub fn subscribe(&self) -> mpsc::Receiver<HubEvent> {
        let (tx, rx) = mpsc::channel();
        lock(&self.inner.subscribers).push(tx);
        rx
    }

    /// The client that owns `block_id`, as last heard on a connection attached to its session.
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

    fn gate(&self, server: &str) -> Arc<Mutex<()>> {
        lock(&self.gates)
            .entry(server.to_owned())
            .or_default()
            .clone()
    }

    fn current(&self, server: &str) -> Option<(Arc<ControlClient>, Endpoint)> {
        self.lock()
            .conns
            .get(server)
            .map(|c| (c.client.clone(), c.endpoint.clone()))
    }

    /// `server`'s connection, opening it if there is none. A label resolves through the local
    /// server's host list; an unreachable local server is started once and retried.
    fn conn(self: &Arc<Self>, server: &str) -> Result<(Arc<ControlClient>, Endpoint), RexError> {
        if let Some(c) = self.current(server) {
            return Ok(c);
        }
        let gate = self.gate(server);
        let _held = lock(&gate);
        if let Some(c) = self.current(server) {
            return Ok(c);
        }
        let endpoint = if server == LOCAL {
            self.local.clone()
        } else {
            let (local, _) = self.conn(LOCAL)?;
            server::resolve(server, &local)?
        };
        let client = match (
            ControlClient::connect(&endpoint, &self.client),
            self.autostart,
        ) {
            (Err(RexError::Unreachable), Some(start)) if server == LOCAL => {
                start()?;
                ControlClient::connect(&endpoint, &self.client)?
            }
            (r, _) => r?,
        };
        let client = self.install(server, endpoint.clone(), client)?;
        Ok((client, endpoint))
    }

    /// Read the session list on a fresh connection (`session.attach_all`, or `session.list` when
    /// it resumed a client that still has it), attach the sessions this server had attached, make
    /// it `server`'s connection and start its watcher.
    fn install(
        self: &Arc<Self>,
        server: &str,
        endpoint: Endpoint,
        client: ControlClient,
    ) -> Result<Arc<ControlClient>, RexError> {
        let events = client.events();
        let (seq, list) = if client.resumed() {
            client.call_seq("session.list", json!({}))?
        } else {
            client.call_seq("session.attach_all", json!({ "event_scope": "session" }))?
        };
        let client = Arc::new(client);
        let token = client.reconnect_token().map(str::to_owned);
        let gen = {
            let mut st = self.lock();
            if let Some(c) = st.conns.get(server) {
                return Ok(c.client.clone());
            }
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
            if let Err(RexError::Api { code, .. }) = self.attach(server, &client, &id) {
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
        Ok(client)
    }

    /// `session.attach` `session_id` on `client`, recording its blocks' owners; returns the view.
    fn attach(
        &self,
        server: &str,
        client: &Arc<ControlClient>,
        session_id: &str,
    ) -> Result<Value, RexError> {
        let (seq, result) =
            client.call_seq("session.attach", json!({ "session_id": session_id }))?;
        let view = result["view"].clone();
        let mut changed = Vec::new();
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            st.attached
                .entry(server.to_owned())
                .or_default()
                .insert(session_id.to_owned());
            if let Some(conn) = st
                .conns
                .get_mut(server)
                .filter(|c| Arc::ptr_eq(&c.client, client))
            {
                for block in view_blocks(&view, None) {
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
        Ok(view)
    }

    /// Apply one event from connection `gen` of `server`; a replaced connection's are ignored.
    fn apply(&self, server: &str, gen: u64, ev: Event) {
        let mut out = Vec::new();
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(conn) = st.conns.get_mut(server).filter(|c| c.gen == gen) else {
                return;
            };
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
        }
        self.emit(out);
    }

    /// Reopen `server`'s connection after a drop, resuming its client when `token` is still
    /// good. `Ok(false)` when another caller already reconnected it.
    fn reconnect(
        self: &Arc<Self>,
        server: &str,
        endpoint: &Endpoint,
        token: Option<&str>,
    ) -> Result<bool, RexError> {
        let gate = self.gate(server);
        let _held = lock(&gate);
        if self.current(server).is_some() {
            return Ok(false);
        }
        let client = match token {
            Some(t) => ControlClient::resume(endpoint, &self.client, t)?,
            None => ControlClient::connect(endpoint, &self.client)?,
        };
        self.install(server, endpoint.clone(), client)?;
        Ok(true)
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
                st.conns.remove(&server);
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

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn str_field(v: &Value, what: &str) -> Result<String, RexError> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| RexError::Protocol(format!("{what} missing")))
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
