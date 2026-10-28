//! A Rex pane is an ordinary surface whose command is `warden-relay`, attached to the pane's Rex
//! session. This builds that command, and carries a pane's open off the main thread and back.
//!
//! The open path: `Registry::ensure_spawned` meets a cold Rex pane, marks it opening and hands
//! [`opener`]'s closure a `RexOpen`. That runs `RexHub::ensure_session` on a worker thread
//! (bounded by the hub's open budget) and posts the answer to the main thread, where [`landed`]
//! gives it to the registry, which spawns the relay in the pane if it still wants one. AppKit's
//! main thread never waits on the network.

use std::path::{Path, PathBuf};

use rex_client::RexError;
use tauri::{AppHandle, Emitter, Manager};

use super::server::LOCAL;
use super::{unclaimed, HubEvent, PaneTarget, RexHub};
use crate::probe::Presence;
use crate::registry::{RexLanded, RexOpen, RexOpener};
use crate::surface::TabSpec;
use crate::ManagerState;

/// The relay binary: `warden-relay` beside the running executable — `target/<profile>/` under
/// `cargo run`, `Contents/MacOS/` in the bundle.
pub fn relay_path() -> PathBuf {
    std::env::current_exe()
        .map(|exe| exe.with_file_name("warden-relay"))
        .unwrap_or_else(|_| PathBuf::from("warden-relay"))
}

/// The command line that runs `relay` on `t`, each argument quoted for the shell libghostty
/// hands it to.
pub fn relay_command(relay: &Path, t: &PaneTarget) -> String {
    let endpoint = t.endpoint.to_string();
    [
        &*relay.to_string_lossy(),
        "--endpoint",
        &endpoint,
        "--session",
        &t.session_id,
        "--block",
        &t.block_id,
    ]
    .map(shell_quote)
    .join(" ")
}

/// `spec` as the surface runs it once its session is `t`: the relay instead of the shell, and
/// nothing typed, since the session's terminal had its `cmd` typed when it was created. A remote
/// tab's `dir` is a path on its server, and an unclaimed session's tab has none, so for those the
/// relay starts at `/` here.
pub fn effective_spec(spec: &TabSpec, t: &PaneTarget) -> TabSpec {
    let mut e = spec.clone();
    e.shell = relay_command(&relay_path(), t);
    e.startup = None;
    let server = spec.server.as_deref();
    if server.is_some_and(|s| s != LOCAL) || unclaimed::is_unclaimed(&spec.id, &spec.dir, server) {
        e.dir = PathBuf::from("/");
    }
    e
}

/// What a tab shows when its pane's session couldn't be opened on `server`.
pub fn open_error(server: &str, e: &RexError) -> String {
    match e {
        RexError::Unreachable => format!("Rex server unreachable: {server}"),
        RexError::Api { code, .. } if code == super::server::HOST_NOT_FOUND => {
            format!("Rex server not found: {server}")
        }
        RexError::Api { message, .. } => message.clone(),
        other => format!("Rex server {server}: {other}"),
    }
}

/// The [`RexOpener`] for window `label`: each open runs on its own worker thread and lands back
/// on the main thread through [`landed`]. The answer's own presence refresh runs on the worker, so
/// a connection a caller (not the hub's watcher) restored still repaints the dots.
pub fn opener(app: AppHandle, label: String) -> RexOpener {
    Box::new(move |req: RexOpen| {
        let (app, label) = (app.clone(), label.clone());
        std::thread::Builder::new()
            .name("warden-rex-open".into())
            .spawn(move || {
                let relay = relay_path();
                let result = match app.try_state::<RexHub>() {
                    // Without the relay the pane's command would fail at once, with no reason
                    // shown; say what's missing instead.
                    _ if !relay.is_file() => Err(format!("{} is missing", relay.display())),
                    Some(hub) => {
                        // An unclaimed session's pane attaches the session it lists.
                        if unclaimed::is_unclaimed(&req.pane_key, &req.dir, Some(&req.server)) {
                            if let Some(id) = unclaimed::session_of(&req.pane_key) {
                                hub.adopt(&req.pane_key, &req.server, id);
                            }
                        }
                        hub.ensure_session(
                            &req.pane_key,
                            &req.server,
                            &req.dir,
                            req.startup.as_deref(),
                            &req.label,
                        )
                        .map_err(|e| open_error(&req.server, &e))
                    }
                    None => Err(format!("Rex server {}: no connection to Rex", req.server)),
                };
                let server = req.server.clone();
                let main = app.clone();
                let _ = app.run_on_main_thread(move || landed(&main, &label, &req, result));
                hub_changed(&app, Some(&server));
            })
            .map(|_| ())
            .map_err(|e| format!("Rex server: couldn't start opening the session: {e}"))
    })
}

/// Give window `label`'s registry the answer to `req` (main thread), then repaint that window's
/// chrome: a fresh snapshot (the row now live, or warning), and the reason on its error banner
/// when the open or the spawn failed. An answer the pane stopped waiting for changes nothing.
fn landed(app: &AppHandle, label: &str, req: &RexOpen, result: Result<PaneTarget, String>) {
    let Some(st) = app.try_state::<ManagerState>() else {
        return;
    };
    let mut m = st.lock();
    let Some(ws) = m.windows.get_mut(label) else {
        return;
    };
    let failure = result.as_ref().err().cloned();
    let message = match ws.registry.rex_opened(&req.pane_key, req.token, result) {
        RexLanded::Stale => return,
        RexLanded::Failed => failure,
        RexLanded::Opened(r) => r.err().map(|e| format!("couldn't open terminal: {e}")),
    };
    m.record_session(label);
    let dto = m.init_dto(label);
    drop(m);
    if let Some(dto) = dto {
        let _ = app.emit_to(label, "warden:refresh", dto);
    }
    if let Some(message) = message {
        eprintln!("warden: {label}: {}: {message}", req.pane_key);
        let _ = app.emit_to(
            label,
            "warden:error",
            serde_json::json!({ "label": label, "message": message }),
        );
    }
}

/// Repaint the presence dot of every Rex tab on `server` (every server when `None`) from the
/// hub's live session lists: its main pane's session alive is `Present`, anything else `Absent`.
/// Locks only; safe from any thread.
pub fn refresh_presence(app: &AppHandle, server: Option<&str>) {
    let (Some(st), Some(hub)) = (app.try_state::<ManagerState>(), app.try_state::<RexHub>()) else {
        return;
    };
    let tabs: Vec<(String, String)> = {
        let m = st.lock();
        m.windows
            .iter()
            .flat_map(|(label, ws)| {
                ws.registry
                    .rex_tabs()
                    .into_iter()
                    .filter(|(_, s)| server.is_none_or(|w| w == s))
                    .map(|(id, _)| (label.clone(), id))
            })
            .collect()
    };
    for (label, id) in tabs {
        let p = if hub.session_alive(&id) {
            Presence::Present
        } else {
            Presence::Absent
        };
        crate::probe::set_presence(app, &label, &id, p);
    }
}

/// A server's session list, or which sessions panes map to, may have changed: repaint its tabs'
/// presence dots and every window's unclaimed sessions. Locks only; safe from any thread but the
/// main one holding `ManagerState`.
pub fn hub_changed(app: &AppHandle, server: Option<&str>) {
    refresh_presence(app, server);
    unclaimed::refresh(app);
}

/// Keep Rex tabs' presence dots and the unclaimed-session lists current for the app's life: every
/// change the hub reports to a server's session list, and every return of its connection,
/// refreshes both ([`hub_changed`]); a drop repaints the dots alone, since the sessions it last
/// listed still exist. It never waits on `Reconnected` alone — a caller can restore a connection
/// first, and then none is sent — which is why each open, ☠ and exit also refreshes.
pub fn watch_presence(app: AppHandle) {
    let Some(hub) = app.try_state::<RexHub>().map(|h| h.inner().clone()) else {
        return;
    };
    let events = hub.subscribe();
    let spawned = std::thread::Builder::new()
        .name("warden-rex-presence".into())
        .spawn(move || {
            for ev in events {
                match ev {
                    HubEvent::SessionsChanged { server } | HubEvent::Reconnected { server } => {
                        hub_changed(&app, Some(&server))
                    }
                    HubEvent::Disconnected { server } => refresh_presence(&app, Some(&server)),
                    HubEvent::OwnerChanged { .. } => {}
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("warden: Rex presence won't update: {e}");
    }
}

/// A Rex pane's program exited (its relay did): let go of its session if its terminal really
/// finished ([`RexHub::destroy_if_exited`]), on a worker, then refresh ([`hub_changed`]). A relay that left for any other reason (the server
/// unreachable, the pane closed) leaves the session for the next open to reattach.
pub fn end_if_exited(app: &AppHandle, pane_key: String) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("warden-rex-exit".into())
        .spawn(move || {
            if let Some(hub) = app.try_state::<RexHub>() {
                hub.destroy_if_exited(&pane_key);
            }
            hub_changed(&app, None);
        });
    if let Err(e) = spawned {
        eprintln!("warden: couldn't start checking whether a Rex session ended: {e}");
    }
}

/// Destroy the Rex sessions of `pane_keys` (☠ on tab `id` of window `label`) on a worker, then
/// refresh ([`hub_changed`]) and burst the window's probes the way a `kill` command's end does.
pub fn destroy(app: &AppHandle, label: String, id: String, pane_keys: Vec<String>) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("warden-rex-destroy".into())
        .spawn(move || {
            if let Some(hub) = app.try_state::<RexHub>() {
                for key in &pane_keys {
                    if let Err(e) = hub.destroy(key) {
                        eprintln!("warden: couldn't end Rex session for {key:?}: {e}");
                    }
                }
            }
            hub_changed(&app, None);
            crate::probe::bump_tab_await(&label, &id, false);
        });
    if let Err(e) = spawned {
        eprintln!("warden: couldn't start ending a tab's Rex sessions: {e}");
    }
}

/// `s` as one shell word: bare when it holds only characters no shell treats specially, else in
/// single quotes.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::TabSpec;
    use rex_client::Endpoint;
    use std::path::{Path, PathBuf};

    fn target() -> PaneTarget {
        PaneTarget {
            endpoint: Endpoint::Unix(PathBuf::from(
                "/Users/x/Library/Application Support/rex/server.sock",
            )),
            session_id: "session:abc".into(),
            block_id: "block:1".into(),
        }
    }

    fn spec(server: &str, dir: &str) -> TabSpec {
        TabSpec {
            id: "t".into(),
            title: "t".into(),
            dir: PathBuf::from(dir),
            shell: "/bin/zsh -l".into(),
            startup: Some("amux".into()),
            group: None,
            probe: None,
            kill: None,
            suspend: None,
            tree: false,
            tree_path: Vec::new(),
            split: None,
            server: Some(server.into()),
        }
    }

    #[test]
    fn relay_command_quotes_paths_with_spaces() {
        let relay = Path::new("/Applications/warden.app/Contents/MacOS/warden-relay");
        assert_eq!(
            relay_command(relay, &target()),
            "/Applications/warden.app/Contents/MacOS/warden-relay \
             --endpoint 'unix:/Users/x/Library/Application Support/rex/server.sock' \
             --session session:abc --block block:1"
        );
    }

    #[test]
    fn relay_command_escapes_single_quotes() {
        let relay = Path::new("/tmp/it's here/warden-relay");
        assert!(relay_command(relay, &target()).starts_with(r"'/tmp/it'\''s here/warden-relay' "));
    }

    #[test]
    fn effective_spec_replaces_shell_and_clears_startup() {
        let s = spec("local", "/work/proj");
        let e = effective_spec(&s, &target());
        assert_eq!(e.shell, relay_command(&relay_path(), &target()));
        assert_eq!(e.startup, None);
        assert_eq!(
            e.dir,
            PathBuf::from("/work/proj"),
            "a local tab keeps its dir"
        );
        assert_eq!((e.id, e.title, e.server), (s.id, s.title, s.server));
    }

    #[test]
    fn effective_spec_runs_a_remote_relay_from_root() {
        // A remote tab's dir is a path on that server; the relay itself needs none here.
        let e = effective_spec(&spec("remote-host", "~/proj"), &target());
        assert_eq!(e.dir, PathBuf::from("/"));
    }

    #[test]
    fn effective_spec_runs_an_unclaimed_sessions_relay_from_root() {
        let mut s = spec(LOCAL, "");
        s.id = "rex:session:x".into();
        assert_eq!(effective_spec(&s, &target()).dir, PathBuf::from("/"));
    }

    #[test]
    fn relay_path_is_beside_the_running_binary() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(relay_path(), exe.with_file_name("warden-relay"));
    }

    #[test]
    fn open_errors_name_the_server() {
        assert_eq!(
            open_error(LOCAL, &RexError::Unreachable),
            "Rex server unreachable: local"
        );
        let not_installed = RexError::Api {
            code: "not_installed".into(),
            message: "Rex isn't installed on this Mac".into(),
            status: 0,
        };
        assert_eq!(
            open_error(LOCAL, &not_installed),
            "Rex isn't installed on this Mac"
        );
        let unknown = crate::rex::server::host_not_found("remote-host");
        assert_eq!(
            open_error("remote-host", &unknown),
            "Rex server not found: remote-host"
        );
        // `session.create` answers `not_found` for a missing creator: the server was found.
        let no_creator = RexError::Api {
            code: "not_found".into(),
            message: "no creator for flavor".into(),
            status: 404,
        };
        assert_eq!(
            open_error("remote-host", &no_creator),
            "no creator for flavor"
        );
    }

    /// Manual, against this Mac's running Rex server (`cargo build -p warden-relay && cargo test
    /// -p warden-app real_server_relay -- --ignored`): open a session the way a tab does, run the
    /// relay through `sh -c` with exactly the command line a pane gets, and see the session's
    /// screen arrive. Touches only a session labelled `warden-spawn-test`, destroyed on the way out.
    #[test]
    #[ignore = "needs a running Rex server and a built warden-relay"]
    fn real_server_relay_command_attaches() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        struct Cleanup(RexHub);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.destroy("warden-spawn-test");
            }
        }
        let hub = Cleanup(RexHub::in_memory(rex_client::ClientInfo {
            kind: "app",
            name: "warden-spawn-test".into(),
            version: "0.0.0".into(),
        }));
        let dir = tempfile::tempdir().unwrap();
        let t = hub
            .0
            .ensure_session(
                "warden-spawn-test",
                LOCAL,
                dir.path(),
                Some("echo spawn-$((6*7))"),
                "warden-spawn-test",
            )
            .expect("open the session");
        // The test binary sits in target/<profile>/deps; the relay one level up.
        let exe = std::env::current_exe().unwrap();
        let relay = exe.parent().unwrap().parent().unwrap().join("warden-relay");
        assert!(relay.is_file(), "build it first: {}", relay.display());
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(relay_command(&relay, &t))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("run the relay");
        let mut out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(200)) {
                seen.extend(chunk);
            }
            if String::from_utf8_lossy(&seen).contains("spawn-42") {
                break;
            }
        }
        let still_running = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        eprintln!(
            "relay wrote {} bytes; saw the typed command's output: {}",
            seen.len(),
            String::from_utf8_lossy(&seen).contains("spawn-42")
        );
        assert!(still_running, "the relay stayed attached");
        assert!(
            !seen.is_empty(),
            "the session's screen reached the relay's stdout"
        );
        drop(hub);
        let c = rex_client::ControlClient::connect(
            &rex_client::Endpoint::local(),
            &rex_client::ClientInfo {
                kind: "app",
                name: "warden-spawn-test".into(),
                version: "0.0.0".into(),
            },
        )
        .expect("connect");
        let list = c.call("session.list", serde_json::json!({})).expect("list");
        assert!(
            !list.to_string().contains("warden-spawn-test"),
            "the test session was destroyed"
        );
    }
}
