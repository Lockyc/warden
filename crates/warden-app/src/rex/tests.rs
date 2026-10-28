use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rex_client::fake::{FakeServer, Reply};
use rex_client::{ClientInfo, Endpoint, RexError};
use serde_json::{json, Value};

use super::store::{PaneRecord, RexStore};
use super::{HubEvent, RexHub};

const WAIT: Duration = Duration::from_secs(3);

fn info() -> ClientInfo {
    ClientInfo {
        kind: "app",
        name: "warden-test".into(),
        version: "0.0.0".into(),
    }
}

fn hub_on(endpoint: Endpoint, dir: &tempfile::TempDir) -> (RexHub, PathBuf) {
    let path = dir.path().join("rex-sessions.json");
    (RexHub::for_test(path.clone(), info(), endpoint), path)
}

fn view(session_id: &str, block_id: &str, owner: Option<&str>) -> Value {
    let mut block = json!({
        "block_id": block_id,
        "creator_name": "com.superlogical.terminal",
        "flavor": "com.superlogical.terminal.shell",
    });
    if let Some(o) = owner {
        block["owner"] = json!(o);
    }
    json!({"client_id": "client:fake-1", "view": {
        "revision": 1, "session_id": session_id, "windows": [{
            "window_id": "window:1",
            "layers": [{"layer_id": "layer:1", "kind": "tiled", "bounds": [0, 0, 1, 1],
                        "layout": {"rect": [0, 0, 1, 1], "block_id": block_id},
                        "blocks": [block]}],
        }],
    }})
}

/// A fake that lists `sessions`, creates `session:new` / `block:new`, and attaches any session
/// with a single terminal block named after it.
fn scripted(sessions: Value) -> FakeServer {
    let fake = FakeServer::start();
    fake.reply("session.attach_all", json!({ "sessions": sessions }));
    fake.reply(
        "session.create",
        json!({"session_id": "session:new", "initial_windows": [
            {"window_id": "window:1", "block_ids": ["block:new"], "revision": 1}
        ]}),
    );
    fake.on("session.attach", |p, _| {
        let id = p["session_id"].as_str().unwrap_or_default();
        Reply::ok(view(id, &format!("block-of-{id}"), None))
    });
    fake
}

fn seed(path: &Path, pane: &str, server: &str, session_id: &str) {
    RexStore::load(path.to_path_buf()).set(
        pane,
        Some(PaneRecord {
            server: server.into(),
            session_id: session_id.into(),
        }),
    );
}

fn next_event(rx: &Receiver<HubEvent>, want: impl Fn(&HubEvent) -> bool) -> HubEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ev) if want(&ev) => return ev,
            Ok(_) => continue,
            Err(e) => panic!("no matching hub event: {e}"),
        }
    }
}

#[test]
fn ensure_session_creates_when_unmapped() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, path) = hub_on(fake.endpoint(), &dir);

    let t = hub
        .ensure_session(
            "tab-a",
            "local",
            Path::new("/work/proj"),
            Some("amux"),
            "proj · main",
        )
        .expect("ensure_session");
    assert_eq!(t.endpoint, fake.endpoint());
    assert_eq!(t.session_id, "session:new");
    assert_eq!(t.block_id, "block:new");

    let creates = fake.requests_for("session.create");
    assert_eq!(creates.len(), 1);
    let p = &creates[0].payload;
    assert_eq!(p["label"], "proj · main");
    let block = &p["initial_windows"][0]["layout"]["block"];
    assert_eq!(block["flavor"], "com.superlogical.terminal.shell");
    assert_eq!(block["options"]["cwd"], "/work/proj");
    assert_eq!(block["options"]["initial_input"], "amux\n");

    assert_eq!(
        RexStore::load(path).get("tab-a"),
        Some(&PaneRecord {
            server: "local".into(),
            session_id: "session:new".into()
        })
    );
    assert!(hub.claimed_session_ids().contains("session:new"));
}

#[test]
fn ensure_session_without_startup_types_nothing() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");
    let creates = fake.requests_for("session.create");
    let options = &creates[0].payload["initial_windows"][0]["layout"]["block"]["options"];
    assert!(options.get("initial_input").is_none(), "{options}");
}

#[test]
fn ensure_session_reattaches_when_mapped_and_alive() {
    let fake = scripted(json!([{"session_id": "session:old", "label": "proj · main"}]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rex-sessions.json");
    seed(&path, "tab-a", "local", "session:old");
    let (hub, _) = hub_on(fake.endpoint(), &dir);

    let t = hub
        .ensure_session(
            "tab-a",
            "local",
            Path::new("/work/proj"),
            Some("amux"),
            "proj · main",
        )
        .expect("ensure_session");
    assert!(fake.requests_for("session.create").is_empty());
    let attaches = fake.requests_for("session.attach");
    assert_eq!(attaches[0].payload["session_id"], "session:old");
    assert_eq!(t.session_id, "session:old");
    assert_eq!(t.block_id, "block-of-session:old");
}

#[test]
fn stale_session_id_recreates() {
    let fake = scripted(json!([]));
    fake.on("session.attach", |p, _| {
        let id = p["session_id"].as_str().unwrap_or_default();
        if id == "session:old" {
            Reply::error("session_not_found", "no such session", 404)
        } else {
            Reply::ok(view(id, "block:new", None))
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rex-sessions.json");
    seed(&path, "tab-a", "local", "session:old");
    let (hub, _) = hub_on(fake.endpoint(), &dir);

    let t = hub
        .ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");
    assert_eq!(fake.requests_for("session.create").len(), 1);
    assert_eq!(t.session_id, "session:new");
    assert_eq!(
        RexStore::load(path)
            .get("tab-a")
            .map(|r| r.session_id.as_str()),
        Some("session:new")
    );
}

#[test]
fn ensure_session_reports_unreachable_server() {
    let dir = tempfile::tempdir().unwrap();
    let missing = Endpoint::Unix(dir.path().join("no-server.sock"));
    let (hub, _) = hub_on(missing, &dir);
    let started = Instant::now();
    let r = hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main");
    assert_eq!(r.err(), Some(RexError::Unreachable));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn destroy_removes_mapping_and_calls_session_destroy() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, path) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    hub.destroy("tab-a").expect("destroy");
    let destroys = fake.requests_for("session.destroy");
    assert_eq!(destroys.len(), 1);
    assert_eq!(destroys[0].payload["session_id"], "session:new");
    assert!(RexStore::load(path).get("tab-a").is_none());
    assert!(hub.claimed_session_ids().is_empty());
    hub.destroy("tab-a")
        .expect("destroying an unmapped pane is a no-op");
    assert_eq!(fake.requests_for("session.destroy").len(), 1);
}

#[test]
fn session_events_emit_sessions_changed() {
    let fake = scripted(json!([{"session_id": "session:a", "label": "a"}]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let rx = hub.subscribe();
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");
    let ids = |hub: &RexHub| -> Vec<String> {
        hub.live_sessions("local")
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    assert_eq!(ids(&hub), ["session:a"]);

    fake.push_event(
        "session_created",
        json!({"session": {"session_id": "session:b", "label": "b"}}),
    );
    next_event(
        &rx,
        |e| matches!(e, HubEvent::SessionsChanged { server } if server == "local"),
    );
    assert_eq!(ids(&hub), ["session:a", "session:b"]);
    let b = hub
        .live_sessions("local")
        .into_iter()
        .find(|s| s.id == "session:b");
    assert_eq!(b.map(|s| s.label), Some("b".to_string()));

    fake.push_event("session_destroyed", json!({"session_id": "session:a"}));
    next_event(&rx, |e| matches!(e, HubEvent::SessionsChanged { .. }));
    assert_eq!(ids(&hub), ["session:b"]);
}

#[test]
fn owner_changes_emit_owner_changed() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let rx = hub.subscribe();
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    fake.push_session_event(
        "session:new",
        "block_owner_changed",
        json!({"block_id": "block:new", "owner": "client:phone", "reason": "take"}),
    );
    let ev = next_event(&rx, |e| matches!(e, HubEvent::OwnerChanged { .. }));
    assert_eq!(
        ev,
        HubEvent::OwnerChanged {
            session_id: "session:new".into(),
            block_id: "block:new".into(),
            owner: Some("client:phone".into()),
        }
    );
    assert_eq!(
        hub.owner("session:new", "block:new"),
        Some("client:phone".into())
    );
}

#[test]
fn dropped_connection_resumes_and_rereads_sessions() {
    let fake = scripted(json!([{"session_id": "session:a", "label": "a"}]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let rx = hub.subscribe();
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    // While the connection is down, session:a goes away and session:c appears.
    fake.reply(
        "session.list",
        json!({"sessions": [{"session_id": "session:c", "label": "c"}]}),
    );
    fake.drop_control();
    next_event(
        &rx,
        |e| matches!(e, HubEvent::Disconnected { server } if server == "local"),
    );
    next_event(
        &rx,
        |e| matches!(e, HubEvent::Reconnected { server } if server == "local"),
    );

    let ids = fake.client_ids();
    assert_eq!(ids.len(), 2, "one reconnect");
    assert_eq!(ids[0], ids[1], "the reconnect resumed the same client");
    assert_eq!(fake.requests_for("session.list").len(), 1);
    let live: Vec<String> = hub
        .live_sessions("local")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(live, ["session:c"]);
    // The claimed session is attached again on the new connection.
    let attaches = fake.requests_for("session.attach");
    assert_eq!(
        attaches.last().map(|r| r.payload["session_id"].clone()),
        Some(json!("session:new"))
    );
}

#[test]
fn block_exited_reads_terminal_process() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    fake.reply(
        "com.superlogical.terminal.process",
        json!({"last_exit": null}),
    );
    assert!(!hub.block_exited("tab-a"), "still running");
    let req = fake.requests_for("com.superlogical.terminal.process");
    assert_eq!(req[0].payload["session_id"], "session:new");
    assert_eq!(req[0].payload["block_id"], "block:new");

    fake.reply(
        "com.superlogical.terminal.process",
        json!({"last_exit": {"exit_code": 0}}),
    );
    assert!(hub.block_exited("tab-a"), "exited");

    fake.fail(
        "com.superlogical.terminal.process",
        "session_not_found",
        "gone",
        404,
    );
    assert!(hub.block_exited("tab-a"), "session gone");
    assert!(!hub.block_exited("never-mapped"));
}

/// Manual, against this Mac's running Rex server (`cargo test -p warden-app real_server --
/// --ignored`): create, reattach, read the process, destroy. Touches only sessions labelled
/// `warden-hub-test`.
#[test]
#[ignore = "needs a running Rex server"]
fn real_server_ensure_reattach_destroy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rex-sessions.json");
    let hub = RexHub::new(path.clone(), info());
    let label = "warden-hub-test";
    let first = hub
        .ensure_session("t", "local", dir.path(), Some("echo hub-$((6*7))"), label)
        .expect("create");
    assert!(hub
        .live_sessions("local")
        .iter()
        .all(|s| s.id != first.session_id || s.label == label));
    let again = RexHub::new(path, info())
        .ensure_session("t", "local", dir.path(), None, label)
        .expect("reattach");
    assert_eq!(
        again, first,
        "a second hub reattaches the persisted session"
    );
    assert!(!hub.block_exited("t"), "the shell is still running");
    hub.destroy("t").expect("destroy");
    assert!(hub.block_exited("t") || hub.claimed_session_ids().is_empty());
}

#[test]
fn resolve_reads_host_endpoints_from_hosts_list() {
    use rex_client::ControlClient;
    let fake = FakeServer::start();
    fake.reply(
        "hosts.list",
        json!({"hash": "h", "hosts": [
            {"label": "Remote-Host", "endpoints": ["remote-host.example.ts.net"]},
            {"label": "other", "endpoints": ["https://other.example:7297"]},
        ]}),
    );
    let c = ControlClient::connect(&fake.endpoint(), &info()).expect("connect");
    assert_eq!(super::server::resolve("local", &c), Ok(Endpoint::local()));
    assert_eq!(
        super::server::resolve("remote-host", &c),
        Ok(Endpoint::Https {
            host: "remote-host.example.ts.net".into(),
            port: 443
        })
    );
    assert_eq!(
        super::server::resolve("other", &c),
        Ok(Endpoint::Https {
            host: "other.example".into(),
            port: 7297
        })
    );
    assert!(matches!(
        super::server::resolve("nowhere", &c),
        Err(RexError::Api { .. })
    ));
}

/// A listener that accepts and never answers, holding each connection open; every accept is
/// reported on the returned receiver.
fn silent<L: Send + 'static, S: Send + 'static>(
    listener: L,
    accept: fn(&L) -> std::io::Result<S>,
) -> Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok(s) = accept(&listener) {
            held.push(s);
            let _ = tx.send(());
        }
    });
    rx
}

#[test]
fn unreachable_remote_answers_within_budget() {
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    let _accepts = silent(tcp, |l| l.accept().map(|(s, _)| s));
    let fake = scripted(json!([]));
    fake.reply(
        "hosts.list",
        json!({"hash": "h", "hosts": [
            {"label": "remote-host", "endpoints": [format!("127.0.0.1:{port}")]}
        ]}),
    );
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);

    let started = Instant::now();
    let r = hub.ensure_session("p", "remote-host", Path::new("/w"), None, "w · main");
    assert_eq!(r.err(), Some(RexError::Unreachable));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn reconnect_in_flight_never_holds_up_a_caller() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let rx = hub.subscribe();
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    // The server goes silent: the socket accepts but never answers, so the watcher's resume
    // hangs in its handshake.
    let sock = fake.socket_path().to_path_buf();
    drop(fake);
    next_event(&rx, |e| matches!(e, HubEvent::Disconnected { .. }));
    let _ = std::fs::create_dir_all(sock.parent().unwrap());
    let _ = std::fs::remove_file(&sock);
    let accepts = silent(
        std::os::unix::net::UnixListener::bind(&sock).unwrap(),
        |l| l.accept().map(|(s, _)| s),
    );
    accepts
        .recv_timeout(WAIT)
        .expect("the watcher's reconnect reached the server");

    let started = Instant::now();
    let r = hub.ensure_session("tab-b", "local", Path::new("/w"), None, "w · main");
    assert_eq!(r.err(), Some(RexError::Unreachable));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(sock.parent().unwrap());
}
