//! An in-process Rex server for tests: a tungstenite server on a Unix socket in a temp dir that
//! speaks the control and data protocols just far enough to script a client against it.
//!
//! - **Control:** every request is recorded ([`FakeHandle::requests`]) and answered by the
//!   handler registered for its method ([`FakeHandle::on`]). An unscripted method answers `{}`,
//!   except `data.connect`, which mints a ticket the data side accepts. Events, close codes and
//!   drops are pushed to every open control connection.
//! - **Data:** a HELLO with a minted ticket opens a new stream (its opening bytes from
//!   [`FakeHandle::on_data_open`]); a RESUME with a stream's latest token replays its log from the
//!   client's count. [`FakeHandle::push_data`], [`FakeHandle::drop_data`] and
//!   [`FakeHandle::close_data`] act on the most recently opened stream.
//!
//! Dropping the [`FakeServer`] stops it and drops every connection.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::Shutdown;
use std::ops::Deref;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::handshake::server::{Request as HttpRequest, Response as HttpResponse};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::{Message, WebSocket};

use crate::wire::{self, Welcome};
use crate::Endpoint;

const POLL: Duration = Duration::from_millis(5);

/// One recorded control request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// `Value::Null` when the request carried no payload.
    pub payload: Value,
    /// The serialized message's length in bytes, as it arrived.
    pub raw_len: usize,
}

/// One recorded data handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataHandshake {
    Hello { ticket: [u8; 16] },
    Resume { token: [u8; 16], received: u64 },
}

/// A scripted answer to a request.
#[derive(Debug, Clone)]
pub struct Reply {
    result: Result<Value, (String, String, u16)>,
    delay: Duration,
}

impl Reply {
    pub fn ok(payload: Value) -> Reply {
        Reply {
            result: Ok(payload),
            delay: Duration::ZERO,
        }
    }

    pub fn error(code: &str, message: &str, status: u16) -> Reply {
        Reply {
            result: Err((code.into(), message.into(), status)),
            delay: Duration::ZERO,
        }
    }

    /// Answers after `delay`, without holding up other requests on the connection.
    pub fn after(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }
}

type Handler = Arc<dyn Fn(&Value, &FakeHandle) -> Reply + Send + Sync>;
type OpenHook = Arc<dyn Fn(usize) -> Vec<u8> + Send + Sync>;

/// The running fake. Derefs to [`FakeHandle`], which carries every scripting method.
pub struct FakeServer {
    handle: FakeHandle,
    dir: PathBuf,
}

/// A cloneable handle onto the fake's state; handlers receive one to push events or data.
#[derive(Clone)]
pub struct FakeHandle {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    shutdown: AtomicBool,
    socket: PathBuf,
}

struct State {
    max_message_bytes: usize,
    nonce: u64,
    handlers: HashMap<String, Handler>,
    requests: Vec<Request>,
    hellos: Vec<Value>,
    client_ids: Vec<String>,
    /// reconnect token → client id
    clients: HashMap<String, String>,
    controls: Vec<(u64, Sender<CtlOut>)>,
    tickets: HashSet<[u8; 16]>,
    on_open: Option<OpenHook>,
    reject_hello: VecDeque<u8>,
    reject_resume: VecDeque<u8>,
    streams: Vec<Stream>,
    handshakes: Vec<DataHandshake>,
    counter: u64,
}

struct Stream {
    token: [u8; 16],
    log: Vec<u8>,
    ended: bool,
    conn: Option<(u64, Sender<DataOut>)>,
}

enum CtlOut {
    Response {
        id: String,
        result: Result<Value, (String, String, u16)>,
    },
    Event(Value),
    Close(u16, String),
    Drop,
}

enum DataOut {
    Bytes(Vec<u8>),
    Close(u16, String),
    Drop,
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

impl FakeServer {
    /// Binds a fresh socket and starts serving. Panics if the socket can't be bound (a test
    /// fixture with nowhere to report to).
    pub fn start() -> FakeServer {
        let dir = std::env::temp_dir().join(format!(
            "rex-fake-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fake: create socket dir");
        let socket = dir.join("s.sock");
        let listener = UnixListener::bind(&socket).expect("fake: bind socket");
        listener
            .set_nonblocking(true)
            .expect("fake: nonblocking listener");
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                max_message_bytes: 1_048_576,
                nonce: 0x5eed_0000_0000_0007,
                handlers: HashMap::new(),
                requests: Vec::new(),
                hellos: Vec::new(),
                client_ids: Vec::new(),
                clients: HashMap::new(),
                controls: Vec::new(),
                tickets: HashSet::new(),
                on_open: None,
                reject_hello: VecDeque::new(),
                reject_resume: VecDeque::new(),
                streams: Vec::new(),
                handshakes: Vec::new(),
                counter: 0,
            }),
            changed: Condvar::new(),
            shutdown: AtomicBool::new(false),
            socket,
        });
        let handle = FakeHandle { shared };
        let h = handle.clone();
        thread::spawn(move || accept_loop(listener, h));
        FakeServer { handle, dir }
    }
}

impl Deref for FakeServer {
    type Target = FakeHandle;
    fn deref(&self) -> &FakeHandle {
        &self.handle
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.shared.shutdown.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl FakeHandle {
    fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.shared.changed.notify_all();
    }

    /// Waits until `done` holds over the state or `timeout` passes, then returns `f`'s view.
    fn wait<T>(
        &self,
        timeout: Duration,
        done: impl Fn(&State) -> bool,
        f: impl Fn(&State) -> T,
    ) -> T {
        let deadline = Instant::now() + timeout;
        let mut st = self.state();
        while !done(&st) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, left)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        f(&st)
    }

    /// The endpoint a client connects to.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.shared.socket.clone())
    }

    pub fn socket_path(&self) -> &Path {
        &self.shared.socket
    }

    // ── control: scripting ──

    /// Answers `method` with `handler(payload, handle)`; replaces any earlier handler for it.
    pub fn on(
        &self,
        method: &str,
        handler: impl Fn(&Value, &FakeHandle) -> Reply + Send + Sync + 'static,
    ) {
        self.state()
            .handlers
            .insert(method.to_owned(), Arc::new(handler));
    }

    /// Always answers `method` with `payload`.
    pub fn reply(&self, method: &str, payload: Value) {
        self.on(method, move |_, _| Reply::ok(payload.clone()));
    }

    /// Always fails `method` with this error.
    pub fn fail(&self, method: &str, code: &str, message: &str, status: u16) {
        let r = Reply::error(code, message, status);
        self.on(method, move |_, _| r.clone());
    }

    /// The welcome's `limits.max_message_bytes` for connections opened from now on. A larger
    /// request closes the connection with 1009, as the real server does.
    pub fn set_max_message_bytes(&self, n: usize) {
        self.state().max_message_bytes = n;
    }

    // ── control: pushing ──

    /// Sends a server-wide event `{type: kind, payload}` to every control connection.
    pub fn push_event(&self, kind: &str, payload: Value) {
        let ev = json!({"type": kind, "payload": payload});
        for (_, tx) in &self.state().controls {
            let _ = tx.send(CtlOut::Event(ev.clone()));
        }
    }

    /// Sends a session event, wrapped in `session_event`.
    pub fn push_session_event(&self, session_id: &str, kind: &str, payload: Value) {
        self.push_event(
            "session_event",
            json!({"session_id": session_id, "event": {"type": kind, "payload": payload}}),
        );
    }

    /// Sends a block's own event (`name`, e.g. `bell`), wrapped in `block_event` + `session_event`.
    pub fn push_block_event(&self, session_id: &str, block_id: &str, name: &str, payload: Value) {
        self.push_session_event(
            session_id,
            "block_event",
            json!({
                "block_id": block_id,
                "creator_name": "com.superlogical.terminal",
                "name": name,
                "payload": payload,
            }),
        );
    }

    /// Closes every control connection with this status and reason.
    pub fn close_control(&self, code: u16, reason: &str) {
        for (_, tx) in &self.state().controls {
            let _ = tx.send(CtlOut::Close(code, reason.to_owned()));
        }
    }

    /// Drops every control connection without a close frame.
    pub fn drop_control(&self) {
        for (_, tx) in &self.state().controls {
            let _ = tx.send(CtlOut::Drop);
        }
    }

    // ── control: inspecting ──

    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<Request> {
        self.state().requests.clone()
    }

    /// The requests for `method` so far.
    pub fn requests_for(&self, method: &str) -> Vec<Request> {
        self.state()
            .requests
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }

    /// Waits until at least `n` requests for `method` have arrived (or `timeout`), then returns them.
    pub fn wait_requests(&self, method: &str, n: usize, timeout: Duration) -> Vec<Request> {
        let count = |st: &State| st.requests.iter().filter(|r| r.method == method).count();
        self.wait(
            timeout,
            |st| count(st) >= n,
            |st| {
                st.requests
                    .iter()
                    .filter(|r| r.method == method)
                    .cloned()
                    .collect()
            },
        )
    }

    /// Every hello received, as JSON.
    pub fn hellos(&self) -> Vec<Value> {
        self.state().hellos.clone()
    }

    /// The client id each welcome carried, in connection order (a resumed client repeats its id).
    pub fn client_ids(&self) -> Vec<String> {
        self.state().client_ids.clone()
    }

    /// The number of control connections currently open.
    pub fn control_connections(&self) -> usize {
        self.state().controls.len()
    }

    /// Waits until exactly `n` control connections are open (or `timeout`); returns the count.
    pub fn wait_control_connections(&self, n: usize, timeout: Duration) -> usize {
        self.wait(timeout, |st| st.controls.len() == n, |st| st.controls.len())
    }

    // ── data ──

    /// The channel nonce every stream's WELCOME carries.
    pub fn nonce(&self) -> u64 {
        self.state().nonce
    }

    /// Mints a ticket a HELLO will accept once (what the default `data.connect` returns).
    pub fn mint_ticket(&self) -> String {
        let mut st = self.state();
        let t = st.fresh(0x7c);
        st.tickets.insert(t);
        wire::encode_ticket(&t)
    }

    /// The bytes a new stream starts with (after its WELCOME), given the stream's index (0 for the
    /// first HELLO). Default: none.
    pub fn on_data_open(&self, f: impl Fn(usize) -> Vec<u8> + Send + Sync + 'static) {
        self.state().on_open = Some(Arc::new(f));
    }

    /// Appends to the latest stream, sending at once if it has a connection; a later RESUME
    /// replays it either way.
    pub fn push_data(&self, bytes: &[u8]) {
        let mut st = self.state();
        if let Some(s) = st.streams.last_mut() {
            s.log.extend_from_slice(bytes);
            if let Some((_, tx)) = &s.conn {
                let _ = tx.send(DataOut::Bytes(bytes.to_vec()));
            }
        }
    }

    /// Drops the latest stream's connection without a close frame; the stream waits for a RESUME.
    pub fn drop_data(&self) {
        let mut st = self.state();
        if let Some(s) = st.streams.last_mut() {
            if let Some((_, tx)) = s.conn.take() {
                let _ = tx.send(DataOut::Drop);
            }
        }
    }

    /// Closes the latest stream's connection with this status; the stream ends (no RESUME).
    pub fn close_data(&self, code: u16, reason: &str) {
        let mut st = self.state();
        if let Some(s) = st.streams.last_mut() {
            s.ended = true;
            if let Some((_, tx)) = s.conn.take() {
                let _ = tx.send(DataOut::Close(code, reason.to_owned()));
            }
        }
    }

    /// REJECTs the next HELLO with `reason`, whatever its ticket.
    pub fn reject_next_hello(&self, reason: u8) {
        self.state().reject_hello.push_back(reason);
    }

    /// REJECTs the next RESUME with `reason`, whatever its token.
    pub fn reject_next_resume(&self, reason: u8) {
        self.state().reject_resume.push_back(reason);
    }

    /// Every data handshake so far, in arrival order.
    pub fn data_handshakes(&self) -> Vec<DataHandshake> {
        self.state().handshakes.clone()
    }

    /// Waits until at least `n` data handshakes have arrived (or `timeout`), then returns them all.
    pub fn wait_data_handshakes(&self, n: usize, timeout: Duration) -> Vec<DataHandshake> {
        self.wait(
            timeout,
            |st| st.handshakes.len() >= n,
            |st| st.handshakes.clone(),
        )
    }
}

impl State {
    /// A fresh 16-byte value, distinct from every other the fake has made.
    fn fresh(&mut self, tag: u8) -> [u8; 16] {
        self.counter += 1;
        let mut b = [tag; 16];
        b[8..].copy_from_slice(&self.counter.to_le_bytes());
        b
    }
}

fn accept_loop(listener: UnixListener, h: FakeHandle) {
    while !h.shared.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let h = h.clone();
                thread::spawn(move || serve(stream, h));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => return,
        }
    }
}

// tungstenite's handshake callback returns its whole HTTP error response by value.
#[allow(clippy::result_large_err)]
fn serve(stream: UnixStream, h: FakeHandle) {
    // An accepted socket inherits the listener's O_NONBLOCK on macOS.
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let mut path = String::new();
    let callback = |req: &HttpRequest, mut resp: HttpResponse| {
        path = req.uri().path().to_owned();
        if let Some(proto) = req.headers().get("Sec-WebSocket-Protocol") {
            resp.headers_mut()
                .insert("Sec-WebSocket-Protocol", proto.clone());
        }
        Ok(resp)
    };
    let Ok(ws) = tungstenite::accept_hdr(stream, callback) else {
        return;
    };
    match path.as_str() {
        "/api/control" => serve_control(ws, h),
        "/api/data" => serve_data(ws, h),
        _ => {}
    }
}

fn is_idle(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

fn close_frame(code: u16, reason: String) -> CloseFrame {
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

fn serve_control(mut ws: WebSocket<UnixStream>, h: FakeHandle) {
    let hello = match ws.read() {
        Ok(Message::Text(t)) => serde_json::from_str::<Value>(&t).unwrap_or(Value::Null),
        _ => return,
    };
    if hello.get("type").and_then(Value::as_str) != Some("hello") {
        let _ = ws.close(Some(close_frame(1002, "protocol error".into())));
        let _ = ws.flush();
        return;
    }
    let conn_id = NEXT_CONN.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    let (welcome, max) = {
        let mut st = h.state();
        st.hellos.push(hello.clone());
        let previous = hello
            .get("reconnect_token")
            .and_then(Value::as_str)
            .and_then(|t| st.clients.remove(t));
        let resumed = previous.is_some();
        let id = previous.unwrap_or_else(|| {
            st.counter += 1;
            format!("client:fake-{}", st.counter)
        });
        st.counter += 1;
        let token = format!("reconnect-{}", st.counter);
        st.clients.insert(token.clone(), id.clone());
        st.client_ids.push(id.clone());
        st.controls.push((conn_id, tx.clone()));
        let revision = hello
            .get("api_revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(7);
        let welcome = json!({
            "type": "welcome",
            "protocol": "rex.control.v1",
            "limits": {"max_message_bytes": st.max_message_bytes},
            "client": {"id": id, "reconnect_token": token, "resumed": resumed},
            "api_revision": revision,
            "api_server_revision": 7,
        });
        (welcome, st.max_message_bytes)
    };
    h.notify();
    if ws.send(Message::text(welcome.to_string())).is_ok()
        && ws.get_ref().set_read_timeout(Some(POLL)).is_ok()
    {
        control_loop(&mut ws, &h, &tx, &rx, max);
    }
    let _ = ws.get_ref().shutdown(Shutdown::Both);
    h.state().controls.retain(|(id, _)| *id != conn_id);
    h.notify();
}

fn control_loop(
    ws: &mut WebSocket<UnixStream>,
    h: &FakeHandle,
    tx: &Sender<CtlOut>,
    rx: &Receiver<CtlOut>,
    max: usize,
) {
    let mut seq: u64 = 0;
    while !h.shared.shutdown.load(Ordering::SeqCst) {
        match ws.read() {
            Ok(Message::Text(t)) => {
                if t.len() > max {
                    let _ = ws.close(Some(close_frame(1009, "read limited".into())));
                    continue;
                }
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v.get("type").and_then(Value::as_str) == Some("request") {
                    handle_request(h, tx, &v, t.len());
                }
            }
            Ok(_) => {}
            Err(e) if is_idle(&e) => {}
            Err(_) => return,
        }
        while let Ok(out) = rx.try_recv() {
            let sent = match out {
                CtlOut::Response { id, result } => {
                    let mut m = json!({"type": "response", "id": id, "seq": seq});
                    match result {
                        Ok(p) => m["payload"] = p,
                        Err((code, message, status)) => {
                            m["error"] = json!({"code": code, "message": message, "status": status})
                        }
                    }
                    ws.write(Message::text(m.to_string()))
                }
                CtlOut::Event(ev) => {
                    seq += 1;
                    let m = json!({"type": "event", "seq": seq, "payload": ev});
                    ws.write(Message::text(m.to_string()))
                }
                CtlOut::Close(code, reason) => ws.close(Some(close_frame(code, reason))),
                CtlOut::Drop => return,
            };
            if matches!(sent, Err(ref e) if !is_idle(e)) {
                return;
            }
        }
        if matches!(ws.flush(), Err(ref e) if !is_idle(e)) {
            return;
        }
    }
}

fn handle_request(h: &FakeHandle, tx: &Sender<CtlOut>, v: &Value, raw_len: usize) {
    let id = v.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
    let method = v
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let payload = v.get("payload").cloned().unwrap_or(Value::Null);
    let handler = {
        let mut st = h.state();
        st.requests.push(Request {
            method: method.clone(),
            payload: payload.clone(),
            raw_len,
        });
        st.handlers.get(&method).cloned()
    };
    h.notify();
    let reply = match handler {
        Some(f) => f(&payload, h),
        None if method == "data.connect" => {
            Reply::ok(json!({"ticket": h.mint_ticket(), "expires_in_ms": 60_000}))
        }
        None => Reply::ok(json!({})),
    };
    let out = CtlOut::Response {
        id,
        result: reply.result,
    };
    if reply.delay.is_zero() {
        let _ = tx.send(out);
    } else {
        let tx = tx.clone();
        thread::spawn(move || {
            thread::sleep(reply.delay);
            let _ = tx.send(out);
        });
    }
}

fn serve_data(mut ws: WebSocket<UnixStream>, h: FakeHandle) {
    let msg = match ws.read() {
        Ok(Message::Binary(b)) => b.to_vec(),
        _ => return,
    };
    let conn_id = NEXT_CONN.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    // Ok((welcome, bytes to send after it)) or Err(reject reason).
    let outcome: Result<(Vec<u8>, Vec<u8>), u8> = {
        let mut st = h.state();
        let nonce = st.nonce;
        let parsed = match msg.as_slice() {
            [0x01, rest @ ..] if rest.len() >= 16 => {
                let mut ticket = [0u8; 16];
                ticket.copy_from_slice(&rest[..16]);
                Some(DataHandshake::Hello { ticket })
            }
            [0x02, rest @ ..] if rest.len() >= 24 => {
                let mut token = [0u8; 16];
                token.copy_from_slice(&rest[..16]);
                let mut n = [0u8; 8];
                n.copy_from_slice(&rest[16..24]);
                Some(DataHandshake::Resume {
                    token,
                    received: u64::from_le_bytes(n),
                })
            }
            _ => None,
        };
        if let Some(hs) = &parsed {
            st.handshakes.push(hs.clone());
        }
        match parsed {
            None => Err(0x05),
            Some(DataHandshake::Hello { ticket }) => {
                if let Some(r) = st.reject_hello.pop_front() {
                    Err(r)
                } else if !st.tickets.remove(&ticket) {
                    Err(0x01)
                } else {
                    let index = st.streams.len();
                    let initial = st.on_open.clone().map(|f| f(index)).unwrap_or_default();
                    let token = st.fresh(0xd7);
                    st.streams.push(Stream {
                        token,
                        log: initial.clone(),
                        ended: false,
                        conn: Some((conn_id, tx.clone())),
                    });
                    Ok((welcome_bytes(token, nonce), initial))
                }
            }
            Some(DataHandshake::Resume { token, received }) => {
                let fresh = st.fresh(0xd7);
                if let Some(r) = st.reject_resume.pop_front() {
                    Err(r)
                } else {
                    match st.streams.iter_mut().find(|s| s.token == token && !s.ended) {
                        None => Err(0x01),
                        Some(s) => match usize::try_from(received) {
                            Ok(n) if n <= s.log.len() => {
                                s.token = fresh;
                                if let Some((_, old)) = s.conn.replace((conn_id, tx.clone())) {
                                    let _ = old.send(DataOut::Drop);
                                }
                                Ok((welcome_bytes(fresh, nonce), s.log[n..].to_vec()))
                            }
                            _ => Err(0x05),
                        },
                    }
                }
            }
        }
    };
    h.notify();
    match outcome {
        Err(reason) => {
            let _ = ws.send(Message::binary(vec![0x82, reason]));
            let _ = ws.close(Some(close_frame(1008, "data handshake rejected".into())));
            let _ = ws.flush();
        }
        Ok((welcome, initial)) => {
            let ok = ws.send(Message::binary(welcome)).is_ok()
                && (initial.is_empty() || ws.send(Message::binary(initial)).is_ok())
                && ws.get_ref().set_read_timeout(Some(POLL)).is_ok();
            if ok {
                data_loop(&mut ws, &h, &rx);
            }
            let mut st = h.state();
            for s in &mut st.streams {
                if s.conn.as_ref().is_some_and(|(id, _)| *id == conn_id) {
                    s.conn = None;
                }
            }
        }
    }
    let _ = ws.get_ref().shutdown(Shutdown::Both);
}

fn welcome_bytes(token: [u8; 16], nonce: u64) -> Vec<u8> {
    wire::encode_welcome(&Welcome {
        token,
        resume_window_ms: 60_000,
        nonce,
    })
}

fn data_loop(ws: &mut WebSocket<UnixStream>, h: &FakeHandle, rx: &Receiver<DataOut>) {
    while !h.shared.shutdown.load(Ordering::SeqCst) {
        match ws.read() {
            Ok(_) => {}
            Err(e) if is_idle(&e) => {}
            Err(_) => return,
        }
        while let Ok(out) = rx.try_recv() {
            let sent = match out {
                DataOut::Bytes(b) => ws.write(Message::binary(b)),
                DataOut::Close(code, reason) => ws.close(Some(close_frame(code, reason))),
                DataOut::Drop => return,
            };
            if matches!(sent, Err(ref e) if !is_idle(e)) {
                return;
            }
        }
        if matches!(ws.flush(), Err(ref e) if !is_idle(e)) {
            return;
        }
    }
}
