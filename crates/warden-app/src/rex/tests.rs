use std::collections::HashSet;
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
    assert!(hub.claimed_session_ids(["tab-a"]).contains("session:new"));
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
    assert!(hub.claimed_session_ids(["tab-a"]).is_empty());
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
    assert!(hub.block_exited("t") || hub.claimed_session_ids(["t"]).is_empty());
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

/// A local server that takes longer to start than anyone waits: sleeps its whole allowance.
fn slow_autostart(limit: Duration) -> Result<(), RexError> {
    std::thread::sleep(limit.min(Duration::from_secs(2)));
    Ok(())
}

#[test]
fn autostart_stays_within_each_callers_budget() {
    let dir = tempfile::tempdir().unwrap();
    let missing = Endpoint::Unix(dir.path().join("no-server.sock"));
    let hub = RexHub::for_test_autostart(
        dir.path().join("rex-sessions.json"),
        info(),
        missing,
        slow_autostart,
    );
    // Two tabs open together: the second waits behind the first caller's connect + autostart.
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        let callers: Vec<_> = ["tab-a", "tab-b"]
            .into_iter()
            .map(|pane| {
                let (hub, barrier) = (&hub, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    let started = Instant::now();
                    let r = hub.ensure_session(pane, "local", Path::new("/w"), None, "w · main");
                    (r.err(), started.elapsed())
                })
            })
            .collect();
        for c in callers {
            let (err, took) = c.join().unwrap();
            assert_eq!(err, Some(RexError::Unreachable));
            assert!(took < Duration::from_secs(3), "a caller took {took:?}");
        }
    });
}

#[test]
fn ensure_session_calls_share_the_open_budget() {
    // A remote whose connection comes up but which never answers `session.attach`: the open
    // must give up inside the tab-open budget, not wait out the 10 s call default.
    let remote = scripted(json!([]));
    let local = scripted(json!([]));
    local.reply(
        "hosts.list",
        json!({"hash": "h", "hosts": [
            {"label": "remote-host", "endpoints": [remote.endpoint().to_string()]}
        ]}),
    );
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(local.endpoint(), &dir);
    hub.ensure_session("p", "remote-host", Path::new("/w"), None, "w · main")
        .expect("first open");
    remote.on("session.attach", |_, _| {
        Reply::ok(json!({})).after(Duration::from_secs(10))
    });

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
fn concurrent_opens_of_one_pane_make_one_session() {
    let fake = scripted(json!([]));
    fake.on("session.create", |_, _| {
        Reply::ok(json!({"session_id": "session:new", "initial_windows": [
            {"window_id": "window:1", "block_ids": ["block:new"], "revision": 1}
        ]}))
        .after(Duration::from_millis(300))
    });
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let barrier = std::sync::Barrier::new(2);
    let targets: Vec<_> = std::thread::scope(|s| {
        let callers: Vec<_> = (0..2)
            .map(|_| {
                let (hub, barrier) = (&hub, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
                })
            })
            .collect();
        callers.into_iter().map(|c| c.join().unwrap()).collect()
    });
    assert_eq!(fake.requests_for("session.create").len(), 1);
    let sessions: Vec<_> = targets
        .into_iter()
        .map(|t| t.expect("both opens answer").session_id)
        .collect();
    assert_eq!(sessions, ["session:new", "session:new"]);
}

#[test]
fn destroy_if_exited_ends_only_a_finished_block() {
    let fake = scripted(json!([]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    fake.reply(
        "com.superlogical.terminal.process",
        json!({"last_exit": null}),
    );
    assert!(!hub.destroy_if_exited("tab-a"));
    assert!(fake.requests_for("session.destroy").is_empty());
    assert!(hub.session_alive("tab-a") || hub.live_sessions("local").is_empty());

    fake.reply(
        "com.superlogical.terminal.process",
        json!({"last_exit": {"exit_code": 0}}),
    );
    assert!(hub.destroy_if_exited("tab-a"));
    assert_eq!(fake.requests_for("session.destroy").len(), 1);
    assert!(hub.claimed_session_ids(["tab-a"]).is_empty());
}

#[test]
fn session_alive_reads_the_live_list() {
    let fake = scripted(json!([{"session_id": "session:new", "label": "w · main"}]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    assert!(!hub.session_alive("tab-a"), "unmapped");
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");
    assert!(hub.session_alive("tab-a"));
    let rx = hub.subscribe();
    fake.push_event("session_destroyed", json!({"session_id": "session:new"}));
    next_event(&rx, |e| matches!(e, HubEvent::SessionsChanged { .. }));
    assert!(!hub.session_alive("tab-a"));
}

/// [`scripted`] whose `session.attach` shows each block owned by `owner(fake)` (read when the
/// attach arrives, so a test can name the hub's own client id), and whose `session.own_block`
/// hands every asked-for block to the caller.
fn owned_by(owner: fn(&rex_client::fake::FakeHandle) -> Option<String>) -> FakeServer {
    let fake = scripted(json!([]));
    fake.on("session.attach", move |p, h| {
        let id = p["session_id"].as_str().unwrap_or_default();
        Reply::ok(view(id, "block:new", owner(h).as_deref()))
    });
    fake.on("session.own_block", |p, h| {
        let me = h.client_ids().last().cloned().unwrap_or_default();
        let blocks: Vec<Value> = p["block_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| json!({"block_id": b, "owner": me}))
            .collect();
        Reply::ok(json!({ "blocks": blocks }))
    });
    fake
}

fn me(h: &rex_client::fake::FakeHandle) -> Option<String> {
    h.client_ids().last().cloned()
}

fn nobody(_: &rex_client::fake::FakeHandle) -> Option<String> {
    None
}

fn phone(_: &rex_client::fake::FakeHandle) -> Option<String> {
    Some("client:phone".into())
}

const RESIZE: &str = "com.superlogical.terminal.resize";

#[test]
fn claim_takes_and_resizes_when_not_owner() {
    let fake = owned_by(nobody);
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    assert_eq!(hub.claim("tab-a", 120, 40, Some((16, 32))), Ok(true));
    let methods: Vec<String> = fake
        .requests()
        .into_iter()
        .map(|r| r.method)
        .filter(|m| m == "session.own_block" || m == RESIZE)
        .collect();
    assert_eq!(methods, ["session.own_block", RESIZE], "take, then resize");
    let own = &fake.requests_for("session.own_block")[0].payload;
    assert_eq!(own["session_id"], "session:new");
    assert_eq!(own["block_ids"], json!(["block:new"]));
    assert_eq!(own["mode"], "take");
    let resize = &fake.requests_for(RESIZE)[0].payload;
    assert_eq!(resize["session_id"], "session:new");
    assert_eq!(resize["block_id"], "block:new");
    assert_eq!(
        resize["args"],
        json!({"columns": 120, "rows": 40, "cell_width_px": 16, "cell_height_px": 32})
    );

    // Without a cell size the resize leaves both cell fields out.
    let fake = owned_by(phone);
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");
    assert_eq!(hub.claim("tab-a", 80, 24, None), Ok(true));
    assert_eq!(fake.requests_for("session.own_block").len(), 1);
    assert_eq!(
        fake.requests_for(RESIZE)[0].payload["args"],
        json!({"columns": 80, "rows": 24})
    );
}

#[test]
fn claim_is_noop_when_already_owner() {
    // The attach shows warden's own client owning the block (it created it).
    let fake = owned_by(me);
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    // Owning it already, warden still sends the size it has never asked for — but no take.
    assert_eq!(hub.claim("tab-a", 120, 40, None), Ok(true));
    assert!(fake.requests_for("session.own_block").is_empty());
    assert_eq!(fake.requests_for(RESIZE).len(), 1);

    // Owner, at this size: every later claim costs no request at all.
    let before = fake.requests().len();
    for _ in 0..5 {
        assert_eq!(hub.claim("tab-a", 120, 40, None), Ok(false));
    }
    assert_eq!(fake.requests().len(), before, "no requests");

    // An unmapped pane has nothing to claim.
    assert_eq!(hub.claim("never-opened", 120, 40, None), Ok(false));
    assert_eq!(fake.requests().len(), before);
}

#[test]
fn owner_changed_event_rearms_claim() {
    let fake = owned_by(nobody);
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let rx = hub.subscribe();
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");
    assert_eq!(hub.claim("tab-a", 120, 40, None), Ok(true));
    assert_eq!(hub.claim("tab-a", 120, 40, None), Ok(false));

    fake.push_session_event(
        "session:new",
        "block_owner_changed",
        json!({"block_id": "block:new", "owner": "client:phone", "reason": "take"}),
    );
    next_event(
        &rx,
        |e| matches!(e, HubEvent::OwnerChanged { owner: Some(o), .. } if o == "client:phone"),
    );

    assert_eq!(hub.claim("tab-a", 120, 40, None), Ok(true));
    assert_eq!(fake.requests_for("session.own_block").len(), 2);
    assert_eq!(fake.requests_for(RESIZE).len(), 2);
}

#[test]
fn resize_if_owner_skips_when_other_client_owns() {
    let fake = owned_by(phone);
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.ensure_session("tab-a", "local", Path::new("/w"), None, "w · main")
        .expect("ensure_session");

    assert_eq!(hub.resize_if_owner("tab-a", 100, 30, None), Ok(false));
    assert!(fake.requests_for(RESIZE).is_empty());
    assert!(fake.requests_for("session.own_block").is_empty());

    // Once warden owns it, a new size goes out, and the same size again doesn't.
    assert_eq!(hub.claim("tab-a", 120, 40, None), Ok(true));
    assert_eq!(hub.resize_if_owner("tab-a", 100, 30, None), Ok(true));
    let last = fake
        .requests_for(RESIZE)
        .last()
        .map(|r| r.payload["args"].clone());
    assert_eq!(last, Some(json!({"columns": 100, "rows": 30})));
    assert_eq!(hub.resize_if_owner("tab-a", 100, 30, None), Ok(false));
    assert_eq!(fake.requests_for(RESIZE).len(), 2);
}

/// Manual, against this Mac's running Rex server (`cargo test -p warden-app real_server_claim --
/// --ignored`): a second client takes the block, warden claims it back at its own size, and the
/// terminal reports that size and warden as owner. Touches only sessions labelled
/// `warden-size-test`.
#[test]
#[ignore = "needs a running Rex server"]
fn real_server_claim_takes_the_size() {
    use rex_client::ControlClient;
    let dir = tempfile::tempdir().unwrap();
    let hub = RexHub::new(dir.path().join("rex-sessions.json"), info());
    let t = hub
        .ensure_session("t", "local", dir.path(), None, "warden-size-test")
        .expect("create");
    let other = ControlClient::connect(&Endpoint::local(), &info()).expect("second client");
    other
        .call("session.attach", json!({ "session_id": t.session_id }))
        .expect("attach");
    let size = |c: &ControlClient| {
        c.block_call(&t.session_id, &t.block_id, "size", Value::Null)
            .expect("terminal.size")
    };
    let result = (|| {
        other.call(
            "session.own_block",
            json!({"session_id": t.session_id, "block_ids": [t.block_id], "mode": "take"}),
        )?;
        other.block_call(
            &t.session_id,
            &t.block_id,
            "resize",
            json!({"columns": 50, "rows": 10}),
        )?;
        std::thread::sleep(Duration::from_millis(300));
        let before = size(&other);
        let claimed = hub.claim("t", 123, 37, Some((14, 28)))?;
        std::thread::sleep(Duration::from_millis(300));
        let again = hub.claim("t", 123, 37, Some((14, 28)))?;
        Ok::<_, RexError>((before, claimed, again, size(&other)))
    })();
    hub.destroy("t").expect("destroy");
    let list = other.call("session.list", json!({})).expect("session.list");
    assert!(
        !list.to_string().contains(&t.session_id),
        "the test session is gone"
    );
    let (before, claimed, again, after) = result.expect("claim round trip");
    eprintln!("before: {before}\nafter: {after}");
    assert_eq!(
        (before["columns"].clone(), before["rows"].clone()),
        (json!(50), json!(10))
    );
    assert!(claimed, "the claim sent");
    assert!(!again, "a second claim at the same size sends nothing");
    assert_eq!(
        (after["columns"].clone(), after["rows"].clone()),
        (json!(123), json!(37))
    );
}

#[test]
fn synthesized_tab_attaches_existing_session() {
    let fake = scripted(json!([{"session_id": "session:orphan", "label": "made elsewhere"}]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, path) = hub_on(fake.endpoint(), &dir);
    hub.connect("local").expect("connect");

    let claimed = hub.claimed_session_ids(["tab-a", "tab-a::2"]);
    let tabs = super::unclaimed::synthesize("local", &hub.live_sessions("local"), &claimed);
    assert_eq!(tabs.len(), 1);
    let tab = &tabs[0];
    hub.adopt(&tab.key, "local", "session:orphan");
    let t = hub
        .ensure_session(&tab.key, "local", &tab.dir, None, &tab.title)
        .expect("attach the unclaimed session");
    assert_eq!(t.session_id, "session:orphan");
    assert_eq!(t.block_id, "block-of-session:orphan");
    assert!(fake.requests_for("session.create").is_empty());
    assert!(
        RexStore::load(path).get(&tab.key).is_none(),
        "an adoption is never persisted"
    );
    assert!(
        hub.claimed_session_ids([tab.key.as_str()]).is_empty(),
        "an open unclaimed session stays in the list it was opened from"
    );
    assert!(hub.session_alive(&tab.key));
}

#[test]
fn an_adopted_session_that_ended_is_never_recreated() {
    let fake = scripted(json!([]));
    fake.on("session.attach", |_, _| {
        Reply::error("session_not_found", "no such session", 404)
    });
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    hub.adopt("rex:session:gone", "local", "session:gone");
    let r = hub.ensure_session("rex:session:gone", "local", Path::new(""), None, "gone");
    assert!(
        matches!(r, Err(RexError::Api { ref code, .. }) if code == "session_not_found"),
        "{r:?}"
    );
    assert!(fake.requests_for("session.create").is_empty());
}

#[test]
fn old_session_becomes_unclaimed() {
    let remote = scripted(json!([]));
    let local = scripted(json!([{"session_id": "session:old", "label": "proj · main"}]));
    local.reply(
        "hosts.list",
        json!({"hash": "h", "hosts": [
            {"label": "remote-host", "endpoints": [remote.endpoint().to_string()]}
        ]}),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rex-sessions.json");
    seed(&path, "t", "local", "session:old");
    let (hub, _) = hub_on(local.endpoint(), &dir);
    let unclaimed = |hub: &RexHub| -> Vec<String> {
        let claimed = hub.claimed_session_ids(["t", "t::2"]);
        super::unclaimed::synthesize("local", &hub.live_sessions("local"), &claimed)
            .into_iter()
            .map(|t| t.key)
            .collect()
    };

    hub.ensure_session("t", "local", Path::new("/w"), None, "w · main")
        .expect("open on local");
    assert!(unclaimed(&hub).is_empty(), "the tab claims it");

    // The tab's `server` changes: its pane now maps to a session on the remote.
    hub.ensure_session("t", "remote-host", Path::new("/w"), None, "w · main")
        .expect("open on the remote");
    assert_eq!(unclaimed(&hub), ["rex:session:old"]);
}

#[test]
fn a_session_whose_tab_left_the_config_is_unclaimed() {
    let fake = scripted(json!([{"session_id": "session:old", "label": "proj · main"}]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rex-sessions.json");
    seed(&path, "deleted-tab", "local", "session:old");
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    assert!(hub.claimed_session_ids(["kept-tab"]).is_empty());
    assert_eq!(
        hub.claimed_session_ids(["deleted-tab"]),
        HashSet::from(["session:old".to_string()])
    );
}

#[test]
fn a_dropped_servers_sessions_stay_listed() {
    let fake = scripted(json!([{"session_id": "session:a", "label": "a"}]));
    let dir = tempfile::tempdir().unwrap();
    let (hub, _) = hub_on(fake.endpoint(), &dir);
    let rx = hub.subscribe();
    hub.connect("local").expect("connect");
    drop(fake);
    next_event(
        &rx,
        |e| matches!(e, HubEvent::Disconnected { server } if server == "local"),
    );
    let ids: Vec<String> = hub
        .live_sessions("local")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(
        ids,
        ["session:a"],
        "a session isn't gone because its server is"
    );
}

/// Manual, against this Mac's running Rex server (`cargo test -p warden-app real_server_unclaimed
/// -- --ignored`): a session made outside warden lists as unclaimed, and its tab's pane attaches
/// it without making another. Touches only a session labelled `warden-unclaimed-test`, made and
/// destroyed here; reads the others' list and nothing more.
#[test]
#[ignore = "needs a running Rex server"]
fn real_server_unclaimed_session_attaches() {
    use rex_client::ControlClient;
    let label = "warden-unclaimed-test";
    let other = ControlClient::connect(&Endpoint::local(), &info()).expect("connect");
    let made = other
        .call(
            "session.create",
            json!({"label": label, "initial_windows": [{"layout": {"block": {
                "flavor": "com.superlogical.terminal.shell", "options": {"cwd": "/tmp"}}}}]}),
        )
        .expect("create the test session");
    let id = made["session_id"].as_str().expect("session id").to_owned();
    struct Cleanup<'a>(&'a ControlClient, String);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self
                .0
                .call("session.destroy", json!({ "session_id": self.1 }));
        }
    }
    let _cleanup = Cleanup(&other, id.clone());
    let count = || -> usize {
        let list = other.call("session.list", json!({})).expect("list");
        list["sessions"]
            .as_array()
            .map(|a| a.iter().filter(|s| s["label"] == label).count())
            .unwrap_or(0)
    };
    assert_eq!(count(), 1);

    let dir = tempfile::tempdir().unwrap();
    let hub = RexHub::new(dir.path().join("rex-sessions.json"), info());
    hub.connect("local").expect("hub connects");
    let tabs = super::unclaimed::synthesize(
        "local",
        &hub.live_sessions("local"),
        &hub.claimed_session_ids(["t"]),
    );
    let tab = tabs
        .iter()
        .find(|t| super::unclaimed::session_of(&t.key) == Some(id.as_str()))
        .expect("the test session lists as unclaimed");
    assert_eq!(tab.title, label);
    assert_eq!(tab.group.as_deref(), Some("Rex · local"));
    hub.adopt(&tab.key, "local", &id);
    let t = hub
        .ensure_session(&tab.key, "local", &tab.dir, None, &tab.title)
        .expect("attach");
    assert_eq!(t.session_id, id);
    assert_eq!(count(), 1, "attaching made no session");
    assert!(hub.claimed_session_ids([tab.key.as_str()]).is_empty());

    hub.destroy(&tab.key).expect("☠ ends it");
    assert_eq!(count(), 0, "the test session is gone");
}
