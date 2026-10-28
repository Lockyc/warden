//! The built `warden-relay` binary against `rex_client::fake`, stdin and stdout piped.

#[allow(dead_code)]
#[path = "../src/vt.rs"]
mod vt;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rex_client::fake::FakeServer;

const SESSION: &str = "session:test";
const BLOCK: &str = "block:test";
const WRITE: &str = "com.superlogical.terminal.write";
const CLEAR: &[u8] = b"\x1b[H\x1b[2J";
const WAIT: Duration = Duration::from_secs(10);

struct Relay {
    child: Child,
    stdin: Option<ChildStdin>,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Relay {
    fn start(endpoint: &str) -> Relay {
        Relay::start_reporting(endpoint, None)
    }

    fn start_reporting(endpoint: &str, report: Option<&Path>) -> Relay {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_warden-relay"));
        cmd.args([
            "--endpoint",
            endpoint,
            "--session",
            SESSION,
            "--block",
            BLOCK,
        ]);
        if let Some(path) = report {
            cmd.arg("--report").arg(path);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn relay");
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("stdout");
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().expect("out").extend_from_slice(&buf[..n]);
            }
        });
        Relay { child, stdin, out }
    }

    fn output(&self) -> Vec<u8> {
        self.out.lock().expect("out").clone()
    }

    fn wait_output(&self, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = self.output();
            if find(&out, needle).is_some() {
                return out;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {:?} in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&out)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin");
        stdin.write_all(bytes).expect("write stdin");
        stdin.flush().expect("flush stdin");
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "relay never exited");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn apc(nonce: u64, json: &str) -> Vec<u8> {
    format!("\x1b_rex1;{nonce};{json}\x1b\\").into_bytes()
}

/// A fake whose every new stream opens with a snapshot of `screens(i)` and its `resize`.
fn fake_with(screens: impl Fn(usize) -> &'static str + Send + Sync + 'static) -> FakeServer {
    let fake = FakeServer::start();
    let nonce = fake.nonce();
    fake.on_data_open(move |i| {
        let mut b = vt::encode_snapshot(screens(i).as_bytes(), 80, 24);
        b.extend(apc(
            nonce,
            r#"{"type":"resize","cols":80,"rows":24,"owner":null}"#,
        ));
        b
    });
    fake
}

fn writes(fake: &FakeServer) -> Vec<String> {
    fake.requests_for(WRITE)
        .iter()
        .map(|r| r.payload["args"]["data"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[test]
fn relay_writes_snapshot_then_stream_without_apcs() {
    let fake = fake_with(|_| "on screen");
    let relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(b"on screen");
    let nonce = fake.nonce();
    let mut live = b"out".to_vec();
    live.extend(apc(nonce, r#"{"type":"theme","foreground":"ffffff"}"#));
    live.extend_from_slice(b"put");
    fake.push_data(&live);

    let out = relay.wait_output(b"output");
    assert!(
        out.starts_with(CLEAR),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(find(&out, b"on screen") < find(&out, b"output"));
    assert!(find(&out, b"rex1;").is_none(), "{out:?}");
    assert!(find(&out, b"GHOSTSNP").is_none(), "{out:?}");
}

#[test]
fn relay_assembles_a_snapshot_split_across_messages() {
    let fake = FakeServer::start();
    let snap = vt::encode_snapshot(b"split screen", 80, 24);
    let half = snap.len() / 2;
    let (head, tail) = (snap[..half].to_vec(), snap[half..].to_vec());
    fake.on_data_open(move |_| head.clone());
    let relay = Relay::start(&fake.endpoint().to_string());
    fake.wait_data_handshakes(1, WAIT);
    thread::sleep(Duration::from_millis(100));
    fake.push_data(&tail);
    fake.push_data(b"after");
    let out = relay.wait_output(b"after");
    assert!(find(&out, b"split screen") < find(&out, b"after"));
}

#[test]
fn relay_forwards_stdin_as_terminal_write_and_drops_replies() {
    let fake = fake_with(|_| "");
    let mut relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(CLEAR);
    relay.type_bytes(b"ls\r\x1b[O");
    let reqs = fake.wait_requests(WRITE, 1, WAIT);
    assert_eq!(reqs[0].payload["session_id"], SESSION);
    assert_eq!(reqs[0].payload["block_id"], BLOCK);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(writes(&fake), vec!["ls\r".to_owned()]);
    assert!(fake
        .requests_for("com.superlogical.terminal.resize")
        .is_empty());
}

#[test]
fn relay_reassembles_utf8_split_across_reads() {
    let fake = fake_with(|_| "");
    let mut relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(CLEAR);
    let e = "é".as_bytes();
    relay.type_bytes(&e[..1]);
    thread::sleep(Duration::from_millis(200));
    relay.type_bytes(&e[1..]);
    relay.type_bytes(b"\r");
    let deadline = Instant::now() + WAIT;
    while writes(&fake).concat() != "é\r" {
        assert!(Instant::now() < deadline, "writes: {:?}", writes(&fake));
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn relay_exits_on_child_exited() {
    let fake = fake_with(|_| "");
    let mut relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(CLEAR);
    fake.push_data(&apc(
        fake.nonce(),
        r#"{"type":"child_exited","exit_code":3}"#,
    ));
    assert_eq!(relay.wait_exit().code(), Some(0));
}

#[test]
fn relay_exits_on_block_closed() {
    let fake = fake_with(|_| "");
    let mut relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(CLEAR);
    fake.close_data(4000, "block closed");
    assert_eq!(relay.wait_exit().code(), Some(0));
}

#[test]
fn relay_restarts_stream_after_refused_resume() {
    let fake = fake_with(|i| if i == 0 { "first" } else { "second" });
    let relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(b"first");
    fake.reject_next_resume(0x01);
    fake.drop_data();

    let out = relay.wait_output(b"second");
    assert_eq!(fake.requests_for("data.connect").len(), 2);
    let first = find(&out, b"first").expect("first");
    let second = find(&out, b"second").expect("second");
    // The restart resets the pane (RIS) before redrawing; the startup redraw only clears.
    let redraw = [b"\x1bc".as_slice(), CLEAR].concat();
    let reset = find(&out[first..], &redraw).map(|i| first + i);
    assert!(
        reset.is_some_and(|c| c < second),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(out.starts_with(CLEAR));
    assert!(find(&out[..first], b"\x1bc").is_none(), "{out:?}");
}

#[test]
fn relay_resumes_a_dropped_stream() {
    let fake = fake_with(|_| "screen");
    let relay = Relay::start(&fake.endpoint().to_string());
    relay.wait_output(b"screen");
    fake.drop_data();
    fake.push_data(b"while away");
    relay.wait_output(b"while away");
    assert_eq!(fake.requests_for("data.connect").len(), 1);
}

#[test]
fn relay_exits_75_when_unreachable() {
    let dir = std::env::temp_dir().join(format!("warden-relay-test-{}", std::process::id()));
    let endpoint = format!("unix:{}", dir.join("nothing.sock").display());
    let mut relay = Relay::start(&endpoint);
    assert_eq!(relay.wait_exit().code(), Some(75));
}

const RESUME_APC: &[u8] = b"\x1b_warden1;resume=Y2xhdWRlIC0tcmVzdW1lIFg=\x1b\\";

#[test]
fn relay_forwards_resume_report() {
    let dir = std::env::temp_dir().join(format!("wr-rep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let sock = dir.join("r.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let fake = fake_with(|_| "");
    let relay = Relay::start_reporting(&fake.endpoint().to_string(), Some(&sock));
    relay.wait_output(CLEAR);
    let mut live = b"before".to_vec();
    live.extend_from_slice(RESUME_APC);
    live.extend_from_slice(b"after");
    fake.push_data(&live);

    let (conn, _) = listener.accept().expect("accept");
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).expect("line");
    let got: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(
        got,
        serde_json::json!({"session": SESSION, "block": BLOCK, "resume": "claude --resume X"})
    );
    let out = relay.wait_output(b"beforeafter");
    assert!(find(&out, b"warden1").is_none(), "{out:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relay_keeps_streaming_when_the_report_socket_is_unreachable() {
    let dir = std::env::temp_dir().join(format!("wr-none-{}", std::process::id()));
    let fake = fake_with(|_| "");
    let relay = Relay::start_reporting(&fake.endpoint().to_string(), Some(&dir.join("no.sock")));
    relay.wait_output(CLEAR);
    let mut live = RESUME_APC.to_vec();
    live.extend_from_slice(b"still here");
    fake.push_data(&live);
    let out = relay.wait_output(b"still here");
    assert!(find(&out, b"warden1").is_none(), "{out:?}");
}
