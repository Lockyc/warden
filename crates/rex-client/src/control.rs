//! The control connection: one per server, carrying JSON requests, responses and events.
//!
//! One I/O thread owns the WebSocket. It reads until the socket would block, routes each
//! `response` to the caller waiting on its id and each `event` to the subscribers, writes what
//! callers queued, then `poll(2)`s the socket and a wake pipe — so a call's request goes out
//! at once rather than on a timer tick, and an idle connection costs no wakeups.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::Message;

use crate::endpoint::{self, io_err, Ws};
use crate::{Endpoint, RexError, API_REVISION, CONTROL_PROTOCOL};

/// How long `call` waits for its response.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the welcome may take after the hello.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
/// The server's own ping schedule, mirrored so a vanished server is noticed from this side too.
const PING_INTERVAL: Duration = crate::SERVER_PING_INTERVAL;
/// Three unanswered pings plus the server's grace: silence this long means the server is gone.
const DEAD_AFTER: Duration =
    Duration::from_secs(3 * PING_INTERVAL.as_secs() + crate::SERVER_PING_WAIT.as_secs());
/// How long a close handshake this side started may take before the socket is dropped.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// The smallest `max_message_bytes` any server allows, used if a welcome omits it.
const MIN_MESSAGE_BYTES: usize = 65_536;
/// Status for a connection that ended without a close frame.
const ABNORMAL: u16 = 1006;

/// What the connecting program is, for the server to show people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// `app`, `cli`, `agent` or `other`.
    pub kind: &'static str,
    pub name: String,
    pub version: String,
}

/// A server event, with `session_event` and `block_event` envelopes unwrapped.
///
/// - A server-wide event: `kind` is its type, `payload` its fields; `session_id` is the
///   payload's own `session_id` if it has one (as `session_destroyed` does).
/// - A session event: `kind` is the inner type, `payload` its fields, `session_id` the session.
/// - A block's own event: `kind` is the block event's `name` (e.g. `bell`), and `payload` is the
///   `block_event`'s fields — `block_id`, `creator_name`, `name`, and the event's own fields
///   under `payload`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub kind: String,
    pub session_id: Option<String>,
    pub payload: Value,
}

impl Event {
    fn parse(msg: &Value) -> Option<Event> {
        let seq = msg.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let outer = msg.get("payload")?;
        let mut kind = outer.get("type")?.as_str()?.to_owned();
        let mut payload = outer.get("payload").cloned().unwrap_or(Value::Null);
        let mut session_id = payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if kind == "session_event" {
            let inner = payload.get("event")?.clone();
            kind = inner.get("type")?.as_str()?.to_owned();
            payload = inner.get("payload").cloned().unwrap_or(Value::Null);
            if kind == "block_event" {
                kind = payload.get("name")?.as_str()?.to_owned();
            }
        } else if kind == "block_event" {
            // Only ever sent inside a session_event; tolerate it bare all the same.
            kind = payload.get("name")?.as_str()?.to_owned();
            session_id = None;
        }
        Some(Event {
            seq,
            kind,
            session_id,
            payload,
        })
    }
}

/// A control connection to one server. Every method takes `&self`, so it can be shared across
/// threads; dropping it closes the connection with a close frame, which ends the server's client
/// (use [`ControlClient::reconnect_token`] with [`ControlClient::resume`] only after a drop).
pub struct ControlClient {
    id: String,
    reconnect_token: Option<String>,
    resumed: bool,
    max_message_bytes: usize,
    shared: Arc<Shared>,
    cmds: Sender<Cmd>,
    wake: UnixStream,
    next_id: AtomicU64,
}

enum Cmd {
    Send(String),
    Close,
}

type Pending = HashMap<String, Sender<Result<(u64, Value), RexError>>>;

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    pending: Pending,
    /// Set once the I/O thread has ended, with the close status.
    closed: Option<u16>,
    subscribers: Vec<Sender<Event>>,
    /// Events that arrived before the first `events()` call; `None` once handed over.
    backlog: Option<Vec<Event>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dispatch(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("response") => {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                if let Some(tx) = self.lock().pending.remove(id) {
                    let _ = tx.send(response_result(&v));
                }
            }
            Some("event") => {
                if let Some(ev) = Event::parse(&v) {
                    let mut st = self.lock();
                    if let Some(backlog) = st.backlog.as_mut() {
                        backlog.push(ev);
                    } else {
                        st.subscribers.retain(|s| s.send(ev.clone()).is_ok());
                    }
                }
            }
            _ => {} // a newer server's message type
        }
    }

    fn finish(&self, code: u16) {
        let mut st = self.lock();
        st.closed = Some(code);
        for (_, tx) in st.pending.drain() {
            let _ = tx.send(Err(RexError::Closed(code)));
        }
        st.subscribers.clear();
    }
}

fn response_result(v: &Value) -> Result<(u64, Value), RexError> {
    match v.get("error").filter(|e| !e.is_null()) {
        Some(e) => Err(RexError::Api {
            code: e
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("internal")
                .to_owned(),
            message: e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            status: e
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok())
                .unwrap_or(500),
        }),
        None => Ok((
            v.get("seq").and_then(Value::as_u64).unwrap_or(0),
            v.get("payload").cloned().unwrap_or(Value::Null),
        )),
    }
}

/// Whether `welcome` is a control welcome. The error names the field that's wrong and never
/// quotes the message, which carries the reconnect token — a secret never to be logged.
fn check_welcome(welcome: &Value) -> Result<(), RexError> {
    let field = |k: &str| welcome.get(k).and_then(Value::as_str);
    if field("type") != Some("welcome") {
        return Err(RexError::Protocol(
            "expected a welcome; the reply's type is missing or different".into(),
        ));
    }
    if field("protocol") != Some(CONTROL_PROTOCOL) {
        return Err(RexError::Protocol(format!(
            "the welcome's protocol is missing or not {CONTROL_PROTOCOL}"
        )));
    }
    Ok(())
}

fn close_code(f: Option<CloseFrame>) -> u16 {
    f.map_or(1005, |f| f.code.into())
}

fn ws_err(e: tungstenite::Error, closed: Option<u16>) -> RexError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            RexError::Closed(closed.unwrap_or(ABNORMAL))
        }
        tungstenite::Error::Io(e) => match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => RexError::Timeout,
            _ => RexError::Closed(closed.unwrap_or(ABNORMAL)),
        },
        e => RexError::Protocol(e.to_string()),
    }
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e) if e.kind() == ErrorKind::WouldBlock)
}

impl ControlClient {
    /// Connects as a new client: hello at API revision 7, then the welcome.
    pub fn connect(endpoint: &Endpoint, client: &ClientInfo) -> Result<ControlClient, RexError> {
        Self::open(endpoint, client, None, None)
    }

    /// [`ControlClient::connect`] that gives up at `deadline`: connecting, the WebSocket upgrade
    /// and the welcome together never outlast it. For callers with a budget, such as a UI path.
    pub fn connect_until(
        endpoint: &Endpoint,
        client: &ClientInfo,
        deadline: Instant,
    ) -> Result<ControlClient, RexError> {
        Self::open(endpoint, client, None, Some(deadline))
    }

    /// [`ControlClient::resume`] that gives up at `deadline`, as [`ControlClient::connect_until`].
    pub fn resume_until(
        endpoint: &Endpoint,
        client: &ClientInfo,
        reconnect_token: &str,
        deadline: Instant,
    ) -> Result<ControlClient, RexError> {
        Self::open(endpoint, client, Some(reconnect_token), Some(deadline))
    }

    /// Connects continuing the client a dropped connection served, with the `reconnect_token`
    /// from its last welcome. If the server no longer knows the token it starts a new client and
    /// [`ControlClient::resumed`] is false.
    pub fn resume(
        endpoint: &Endpoint,
        client: &ClientInfo,
        reconnect_token: &str,
    ) -> Result<ControlClient, RexError> {
        Self::open(endpoint, client, Some(reconnect_token), None)
    }

    fn open(
        endpoint: &Endpoint,
        client: &ClientInfo,
        reconnect_token: Option<&str>,
        deadline: Option<Instant>,
    ) -> Result<ControlClient, RexError> {
        let mut ws = endpoint::open_until(endpoint, "/api/control", CONTROL_PROTOCOL, deadline)?;
        let mut hello = json!({
            "type": "hello",
            "api_revision": API_REVISION,
            "client": {
                "kind": client.kind,
                "platform": platform(),
                "name": client.name,
                "version": client.version,
            },
        });
        if let Some(t) = reconnect_token {
            hello["reconnect_token"] = json!(t);
        }
        ws.send(Message::text(hello.to_string()))
            .map_err(|e| ws_err(e, None))?;

        let welcome_limit = match deadline {
            Some(_) => endpoint::step(deadline)?.min(WELCOME_TIMEOUT),
            None => WELCOME_TIMEOUT,
        };
        ws.set_read_timeout(Some(welcome_limit)).map_err(io_err)?;
        let mut closed = None;
        let welcome: Value = loop {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    break serde_json::from_str(&t)
                        .map_err(|e| RexError::Protocol(format!("welcome: {e}")))?
                }
                Ok(Message::Close(f)) => closed = Some(close_code(f)),
                Ok(_) => {}
                Err(e) => return Err(ws_err(e, closed)),
            }
        };
        check_welcome(&welcome)?;
        let max_message_bytes = welcome
            .pointer("/limits/max_message_bytes")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(MIN_MESSAGE_BYTES);
        let field = |k: &str| {
            welcome
                .get("client")
                .and_then(|c| c.get(k))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let id = field("id").unwrap_or_default();
        let token = field("reconnect_token");
        let resumed = welcome
            .pointer("/client/resumed")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        ws.set_read_timeout(None).map_err(io_err)?;
        ws.set_nonblocking(true).map_err(io_err)?;
        let (wake, wake_rx) = UnixStream::pair().map_err(io_err)?;
        wake.set_nonblocking(true).map_err(io_err)?;
        wake_rx.set_nonblocking(true).map_err(io_err)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                backlog: Some(Vec::new()),
                ..State::default()
            }),
        });
        let (cmds, cmd_rx) = mpsc::channel();
        let io_shared = shared.clone();
        thread::Builder::new()
            .name("rex-control".into())
            .spawn(move || {
                let code = io_loop(&mut ws, &cmd_rx, &wake_rx, &io_shared);
                io_shared.finish(code);
            })
            .map_err(io_err)?;

        Ok(ControlClient {
            id,
            reconnect_token: token,
            resumed,
            max_message_bytes,
            shared,
            cmds,
            wake,
            next_id: AtomicU64::new(1),
        })
    }

    /// This client's id (the welcome's `client.id`); stays the same across a resume.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The token that continues this client from a new connection after this one drops.
    pub fn reconnect_token(&self) -> Option<&str> {
        self.reconnect_token.as_deref()
    }

    /// True if this connection continued an existing client.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// The largest message the server accepts on this connection.
    pub fn max_message_bytes(&self) -> usize {
        self.max_message_bytes
    }

    /// The close status once the connection has ended, `None` while it is open.
    pub fn closed(&self) -> Option<u16> {
        self.shared.lock().closed
    }

    fn wake(&self) {
        let _ = (&self.wake).write(&[1]);
    }

    /// Calls `method` and waits up to 10 s for its result. A failed call is `RexError::Api`.
    pub fn call(&self, method: &str, payload: Value) -> Result<Value, RexError> {
        self.call_seq(method, payload).map(|(_, p)| p)
    }

    /// [`ControlClient::call`], also returning the response's `seq`: the last event the server
    /// had queued before it started the request. The result already includes events up to it,
    /// so skip events with `seq <= ` it and apply later ones on top.
    pub fn call_seq(&self, method: &str, payload: Value) -> Result<(u64, Value), RexError> {
        self.call_seq_until(method, payload, Instant::now() + CALL_TIMEOUT)
    }

    /// [`ControlClient::call_seq`] waiting for the response only until `deadline`
    /// (`Timeout` after it).
    pub fn call_seq_until(
        &self,
        method: &str,
        payload: Value,
        deadline: Instant,
    ) -> Result<(u64, Value), RexError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let mut msg = json!({"type": "request", "id": id, "method": method});
        if !payload.is_null() {
            msg["payload"] = payload;
        }
        let text = msg.to_string();
        if text.len() > self.max_message_bytes {
            // The server would close the whole connection (1009) over it.
            return Err(RexError::Protocol(format!(
                "{method} request of {} bytes exceeds the {}-byte message limit",
                text.len(),
                self.max_message_bytes
            )));
        }
        let (tx, rx) = mpsc::channel();
        {
            let mut st = self.shared.lock();
            if let Some(code) = st.closed {
                return Err(RexError::Closed(code));
            }
            st.pending.insert(id.clone(), tx);
        }
        if self.cmds.send(Cmd::Send(text)).is_err() {
            self.shared.lock().pending.remove(&id);
            return Err(RexError::Closed(self.closed().unwrap_or(ABNORMAL)));
        }
        self.wake();
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                self.shared.lock().pending.remove(&id);
                Err(RexError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(RexError::Closed(self.closed().unwrap_or(ABNORMAL)))
            }
        }
    }

    /// Calls the terminal block method `com.superlogical.terminal.<method>` with `args` (left
    /// out when `Null`).
    pub fn block_call(
        &self,
        session: &str,
        block: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, RexError> {
        let mut payload = json!({"session_id": session, "block_id": block});
        if !args.is_null() {
            payload["args"] = args;
        }
        self.call(&format!("com.superlogical.terminal.{method}"), payload)
    }

    /// Types `bytes` into the terminal with `terminal.write`, split into as many calls as keep
    /// each request under the server's message limit, sent one after another so the text arrives
    /// in order. The protocol carries text, so invalid UTF-8 becomes U+FFFD: pass whole
    /// characters (hold back a sequence cut off at the end of a read).
    pub fn write_input(&self, session: &str, block: &str, bytes: &[u8]) -> Result<(), RexError> {
        let text = String::from_utf8_lossy(bytes);
        if text.is_empty() {
            return Ok(());
        }
        let envelope = json!({
            "type": "request",
            "id": u64::MAX.to_string(),
            "method": "com.superlogical.terminal.write",
            "payload": {"session_id": session, "block_id": block, "args": {"data": ""}},
        })
        .to_string()
        .len();
        let budget = self.max_message_bytes.saturating_sub(envelope);
        if budget < 6 {
            return Err(RexError::Protocol(
                "message limit leaves no room for input".into(),
            ));
        }
        for chunk in chunks(&text, budget) {
            self.block_call(session, block, "write", json!({"data": chunk}))?;
        }
        Ok(())
    }

    /// Stop holding events for a first [`ControlClient::events`] call that will never come: the
    /// ones held are dropped, and later ones reach only receivers that already exist. For a
    /// client that never reads events, so a long-lived connection doesn't grow without bound.
    pub fn ignore_events(&self) {
        self.shared.lock().backlog = None;
    }

    /// A new receiver for this connection's events. The first one also gets every event that
    /// arrived since connecting; later ones get events from now on. It disconnects when the
    /// connection ends.
    pub fn events(&self) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        let mut st = self.shared.lock();
        for ev in st.backlog.take().unwrap_or_default() {
            let _ = tx.send(ev);
        }
        if st.closed.is_none() {
            st.subscribers.push(tx);
        }
        rx
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.cmds.send(Cmd::Close);
        self.wake();
    }
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

/// How many bytes `c` takes inside a JSON string, as serde_json escapes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Splits `text` on character boundaries into pieces whose JSON-escaped size fits `budget`.
fn chunks(text: &str, budget: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut size = 0;
    for (i, c) in text.char_indices() {
        let n = escaped_len(c);
        if size + n > budget {
            out.push(&text[start..i]);
            start = i;
            size = 0;
        }
        size += n;
    }
    out.push(&text[start..]);
    out
}

/// Runs the connection until it ends; returns its close status.
fn io_loop(ws: &mut Ws, cmds: &Receiver<Cmd>, wake: &UnixStream, shared: &Shared) -> u16 {
    let Ok(fd) = ws.raw_fd() else {
        return ABNORMAL;
    };
    let mut closed: Option<u16> = None;
    let mut closing_since: Option<Instant> = None;
    let mut wake_open = true;
    let mut last_rx = Instant::now();
    let mut next_ping = last_rx + PING_INTERVAL;
    loop {
        // Everything readable now.
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    last_rx = Instant::now();
                    shared.dispatch(&t);
                }
                Ok(Message::Close(f)) => {
                    last_rx = Instant::now();
                    closed.get_or_insert(close_code(f));
                }
                Ok(_) => last_rx = Instant::now(),
                Err(e) if would_block(&e) => break,
                Err(_) => return closed.unwrap_or(ABNORMAL),
            }
        }
        // Everything callers queued.
        loop {
            match cmds.try_recv() {
                Ok(Cmd::Send(text)) => {
                    if closing_since.is_none() && closed.is_none() {
                        match ws.write(Message::text(text)) {
                            Ok(()) => {}
                            Err(e) if would_block(&e) => {} // queued; flushed below
                            Err(_) => return closed.unwrap_or(ABNORMAL),
                        }
                    }
                }
                Ok(Cmd::Close) | Err(TryRecvError::Disconnected) => {
                    if closing_since.is_none() {
                        closing_since = Some(Instant::now());
                        let _ = ws.close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "".into(),
                        }));
                    }
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let now = Instant::now();
        if now >= next_ping && closing_since.is_none() && closed.is_none() {
            let _ = ws.write(Message::Ping(Default::default()));
            next_ping = now + PING_INTERVAL;
        }
        if now.duration_since(last_rx) > DEAD_AFTER {
            return closed.unwrap_or(ABNORMAL);
        }
        if closing_since.is_some_and(|t| now.duration_since(t) > CLOSE_GRACE) {
            return closed.unwrap_or(1000);
        }
        let want_write = match ws.flush() {
            Ok(()) => false,
            Err(e) if would_block(&e) => true,
            Err(_) => return closed.unwrap_or(ABNORMAL),
        };

        let timeout = if closing_since.is_some() {
            Duration::from_millis(50)
        } else {
            next_ping.saturating_duration_since(now)
        };
        let mut fds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN | if want_write { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: if wake_open { wake.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: `fds` is a valid array of two pollfd for the duration of the call.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, ms.max(1)) };
        if wake_open {
            let mut buf = [0u8; 64];
            loop {
                match (&*wake).read(&mut buf) {
                    Ok(0) => {
                        wake_open = false; // the client is gone; Disconnected closes us
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeServer, Reply};
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    fn info() -> ClientInfo {
        ClientInfo {
            kind: "app",
            name: "warden-test".into(),
            version: "0.0.1".into(),
        }
    }

    #[test]
    fn connect_sends_hello_rev7_and_reads_client_id() {
        let fake = FakeServer::start();
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let hellos = fake.hellos();
        assert_eq!(hellos.len(), 1);
        let h = &hellos[0];
        assert_eq!(h["api_revision"], 7);
        assert_eq!(h["client"]["kind"], "app");
        assert_eq!(h["client"]["name"], "warden-test");
        assert_eq!(h["client"]["version"], "0.0.1");
        assert!(h.get("reconnect_token").is_none());
        assert_eq!(c.id(), fake.client_ids()[0]);
        assert!(c.id().starts_with("client:"));
    }

    /// A listener that accepts connections and never answers, holding them open.
    fn silent<L: Send + 'static, S: Send + 'static>(
        listener: L,
        accept: fn(&L) -> std::io::Result<S>,
    ) {
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok(s) = accept(&listener) {
                held.push(s);
            }
        });
    }

    #[test]
    fn connect_until_gives_up_at_the_deadline() {
        let dir = std::env::temp_dir().join(format!("rex-silent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        silent(
            std::os::unix::net::UnixListener::bind(&sock).unwrap(),
            |l| l.accept().map(|(s, _)| s),
        );
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        silent(tcp, |l| l.accept().map(|(s, _)| s));

        for ep in [
            Endpoint::Unix(sock.clone()),
            Endpoint::Https {
                host: "127.0.0.1".into(),
                port,
            },
        ] {
            let started = Instant::now();
            let r =
                ControlClient::connect_until(&ep, &info(), started + Duration::from_millis(300));
            assert!(r.is_err(), "{ep}: connected to a silent listener");
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "{ep}: took {:?}",
                started.elapsed()
            );
            let r = ControlClient::resume_until(
                &ep,
                &info(),
                "token",
                Instant::now() + Duration::from_millis(300),
            );
            assert!(r.is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn call_seq_until_stops_waiting_at_the_deadline() {
        let fake = FakeServer::start();
        fake.on("slow", |_, _| {
            Reply::ok(json!({})).after(Duration::from_secs(2))
        });
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let started = Instant::now();
        let r = c.call_seq_until("slow", json!({}), started + Duration::from_millis(200));
        assert_eq!(r, Err(RexError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn call_matches_responses_by_id_out_of_order() {
        let fake = FakeServer::start();
        fake.on("slow", |_, _| {
            Reply::ok(json!({"which": "slow"})).after(Duration::from_millis(300))
        });
        fake.reply("fast", json!({"which": "fast"}));
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let order = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            s.spawn(|| {
                let r = c.call("slow", Value::Null).expect("slow");
                assert_eq!(r["which"], "slow");
                order.lock().unwrap().push("slow");
            });
            assert_eq!(fake.wait_requests("slow", 1, WAIT).len(), 1);
            let r = c.call("fast", json!({})).expect("fast");
            assert_eq!(r["which"], "fast");
            order.lock().unwrap().push("fast");
        });
        assert_eq!(*order.lock().unwrap(), ["fast", "slow"]);
    }

    #[test]
    fn call_seq_returns_response_seq() {
        let fake = FakeServer::start();
        fake.reply("session.attach", json!({"view": "v"}));
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let events = c.events();
        assert_eq!(
            c.call_seq("session.attach", json!({}))
                .expect("before events"),
            (0, json!({"view": "v"}))
        );
        fake.push_event("hosts_changed", json!({}));
        fake.push_event("hosts_changed", json!({}));
        for _ in 0..2 {
            events.recv_timeout(WAIT).expect("event");
        }
        assert_eq!(
            c.call_seq("session.attach", json!({}))
                .expect("after events"),
            (2, json!({"view": "v"}))
        );
    }

    #[test]
    fn call_surfaces_api_error_code() {
        let fake = FakeServer::start();
        fake.fail("session.view", "not_found", "block not found", 404);
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        assert_eq!(
            c.call("session.view", json!({"session_id": "session:x"})),
            Err(RexError::Api {
                code: "not_found".into(),
                message: "block not found".into(),
                status: 404,
            })
        );
    }

    #[test]
    fn a_client_ignoring_events_holds_none() {
        let fake = FakeServer::start();
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        c.ignore_events();
        for i in 0..50 {
            fake.push_event("session_destroyed", json!({"session_id": format!("s:{i}")}));
        }
        // A round trip after the pushes: the I/O thread has read every event before the answer.
        c.call("ping", json!({})).expect("round trip");
        assert_eq!(c.shared.lock().backlog.as_ref().map_or(0, Vec::len), 0);
        assert_eq!(c.shared.lock().subscribers.len(), 0);
    }

    #[test]
    fn a_malformed_welcome_never_reports_its_token() {
        let welcome = json!({"type": "welcome", "protocol": "rex.other.v9",
            "client": {"id": "client:1", "reconnect_token": "SECRET-TOKEN"}});
        let e = check_welcome(&welcome).expect_err("wrong protocol");
        let msg = e.to_string();
        assert!(!msg.contains("SECRET-TOKEN"), "{msg}");
        assert!(msg.contains("protocol"), "names the field: {msg}");
        let not_one = json!({"type": "error", "reconnect_token": "SECRET-TOKEN"});
        let msg = check_welcome(&not_one)
            .expect_err("not a welcome")
            .to_string();
        assert!(!msg.contains("SECRET-TOKEN"), "{msg}");
        assert!(msg.contains("type"), "names the field: {msg}");
        let ok = json!({"type": "welcome", "protocol": CONTROL_PROTOCOL});
        assert_eq!(check_welcome(&ok), Ok(()));
    }

    #[test]
    fn events_unwrap_session_and_block_envelopes() {
        let fake = FakeServer::start();
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let events = c.events();
        fake.push_block_event(
            "session:1",
            "block:9",
            "child_exited",
            json!({"exit_code": 0}),
        );
        fake.push_session_event(
            "session:1",
            "block_owner_changed",
            json!({"block_id": "block:9", "owner": "client:x"}),
        );
        fake.push_event("session_destroyed", json!({"session_id": "session:1"}));

        let e = events.recv_timeout(WAIT).expect("block event");
        assert_eq!(e.seq, 1);
        assert_eq!(e.kind, "child_exited");
        assert_eq!(e.session_id.as_deref(), Some("session:1"));
        assert_eq!(e.payload["block_id"], "block:9");
        assert_eq!(e.payload["payload"]["exit_code"], 0);

        let e = events.recv_timeout(WAIT).expect("session event");
        assert_eq!(e.seq, 2);
        assert_eq!(e.kind, "block_owner_changed");
        assert_eq!(e.session_id.as_deref(), Some("session:1"));
        assert_eq!(e.payload["owner"], "client:x");

        let e = events.recv_timeout(WAIT).expect("server event");
        assert_eq!(e.kind, "session_destroyed");
        assert_eq!(e.session_id.as_deref(), Some("session:1"));
    }

    #[test]
    fn write_chunks_under_message_limit() {
        let fake = FakeServer::start();
        fake.set_max_message_bytes(65536);
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        assert_eq!(c.max_message_bytes(), 65536);
        // Escapes and multi-byte characters, so JSON escaping and char boundaries both matter.
        let unit = "ls -la \"x\"\\\t\x1b[A ✓\r\n";
        let mut input = String::new();
        while input.len() + unit.len() <= 200_000 {
            input.push_str(unit);
        }
        while input.len() < 200_000 {
            input.push('x');
        }
        assert_eq!(input.len(), 200_000);

        c.write_input("session:1", "block:1", input.as_bytes())
            .expect("write");

        let writes = fake.requests_for("com.superlogical.terminal.write");
        assert!(writes.len() >= 4, "{} writes", writes.len());
        let mut joined = String::new();
        for w in &writes {
            assert!(w.raw_len <= 65536, "message of {} bytes", w.raw_len);
            assert_eq!(w.payload["session_id"], "session:1");
            assert_eq!(w.payload["block_id"], "block:1");
            joined.push_str(w.payload["args"]["data"].as_str().expect("data"));
        }
        assert_eq!(joined, input);
    }

    #[test]
    fn block_call_prefixes_terminal_methods() {
        let fake = FakeServer::start();
        fake.reply(
            "com.superlogical.terminal.process",
            json!({"exited": false}),
        );
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let r = c
            .block_call("session:1", "block:1", "process", Value::Null)
            .expect("process");
        assert_eq!(r["exited"], false);
        let reqs = fake.requests_for("com.superlogical.terminal.process");
        assert_eq!(
            reqs[0].payload,
            json!({"session_id": "session:1", "block_id": "block:1"})
        );
    }

    #[test]
    fn close_fails_pending_calls_and_ends_events() {
        let fake = FakeServer::start();
        fake.on("hang", |_, _| {
            Reply::ok(json!({})).after(Duration::from_secs(30))
        });
        let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let events = c.events();
        std::thread::scope(|s| {
            let pending = s.spawn(|| c.call("hang", Value::Null));
            fake.wait_requests("hang", 1, WAIT);
            fake.close_control(1001, "keepalive timeout");
            assert_eq!(pending.join().unwrap(), Err(RexError::Closed(1001)));
        });
        assert!(events.recv_timeout(WAIT).is_err());
        assert_eq!(c.closed(), Some(1001));
        assert_eq!(c.call("x", Value::Null), Err(RexError::Closed(1001)));
    }

    #[test]
    fn resume_keeps_client_id() {
        let fake = FakeServer::start();
        let first = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
        let token = first.reconnect_token().expect("token").to_owned();
        let id = first.id().to_owned();
        assert!(!first.resumed());
        let second = ControlClient::resume(&fake.endpoint(), &info(), &token).expect("resume");
        assert_eq!(second.id(), id);
        assert!(second.resumed());
        assert_eq!(fake.hellos()[1]["reconnect_token"], token.as_str());
    }

    /// A manual check against the real local server: `cargo test -p rex-client -- --ignored`.
    #[test]
    #[ignore = "needs a running local Rex server"]
    fn real_server_version() {
        let c = ControlClient::connect(&Endpoint::local(), &info()).expect("connect");
        let v = c
            .call("server.version", Value::Null)
            .expect("server.version");
        eprintln!("client {} — server.version: {v}", c.id());
        // Sweep up a `rex-client-test` session a failed round-trip test left behind.
        let list = c.call("session.list", Value::Null).expect("session.list");
        for s in list["sessions"].as_array().into_iter().flatten() {
            if s["label"] == "rex-client-test" {
                eprintln!("destroying leftover {}", s["session_id"]);
                c.call("session.attach", json!({"session_id": s["session_id"]}))
                    .expect("attach");
                c.call("session.destroy", json!({"session_id": s["session_id"]}))
                    .expect("destroy");
            }
        }
    }
}
