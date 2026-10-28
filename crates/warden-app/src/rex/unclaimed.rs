//! Rex sessions no tab claims, listed in the sidebar so none is ever hidden.
//!
//! Each window with a Rex tab lists, under a `Rex · <server>` group, the sessions on every server
//! warden has heard from that no configured pane maps to: made in Rex.app or on the phone, left behind when a
//! tab moved server or left the config, or orphaned by an open that timed out. Each is an
//! ordinary [`Tab`] keyed `rex:<session id>`, with no `dir`: opening it seeds the hub in memory
//! (`RexHub::adopt`) so its pane attaches that session and never creates one, and nothing is
//! written to config. It stays listed while open, and leaves when the session ends.

use std::collections::HashSet;
use std::path::Path;

use tauri::{AppHandle, Manager};
use warden_config::{Config, Tab};

use super::{RexHub, SessionInfo};
use crate::ManagerState;

/// What an unclaimed session's tab key starts with; the session id follows.
const KEY_PREFIX: &str = "rex:";

/// The sidebar group `server`'s unclaimed sessions are listed under.
pub fn group(server: &str) -> String {
    format!("Rex · {server}")
}

/// A tab for each session in `live` (on `server`) that `claimed` doesn't hold, in `live`'s order.
pub fn synthesize(server: &str, live: &[SessionInfo], claimed: &HashSet<String>) -> Vec<Tab> {
    live.iter()
        .filter(|s| !claimed.contains(&s.id))
        .map(|s| Tab {
            id: None,
            key: format!("{KEY_PREFIX}{}", s.id),
            title: if s.label.is_empty() {
                s.id.clone()
            } else {
                s.label.clone()
            },
            dir: Default::default(),
            shell: String::new(),
            startup: None,
            load_on_open: false,
            group: Some(group(server)),
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: Some(server.to_owned()),
            split: None,
        })
        .collect()
}

/// Whether a tab keyed `key` with `dir` on `server` is an unclaimed session's: a Rex tab with no
/// `dir`, keyed by its session. Such a pane only ever attaches.
pub fn is_unclaimed(key: &str, dir: &Path, server: Option<&str>) -> bool {
    server.is_some() && dir.as_os_str().is_empty() && key.starts_with(KEY_PREFIX)
}

/// The session an unclaimed session's pane key names.
pub fn session_of(pane_key: &str) -> Option<&str> {
    pane_key.strip_prefix(KEY_PREFIX)
}

/// Replace `config`'s unclaimed-session tabs with the current ones: in each window with a Rex
/// tab, for each server its own tabs use (in tab order) and then each other of `connected`,
/// [`synthesize`] over `live(server)`, skipping a key the window already has. Every connected
/// server, not only the window's own, so a session left on a server no tab uses any more still
/// shows.
pub fn add_unclaimed(
    config: &mut Config,
    connected: &[String],
    live: impl Fn(&str) -> Vec<SessionInfo>,
    claimed: &HashSet<String>,
) {
    for window in &mut config.windows {
        window
            .tabs
            .retain(|t| !is_unclaimed(&t.key, &t.dir, t.server.as_deref()));
        let mut servers: Vec<String> = Vec::new();
        for s in window.tabs.iter().filter_map(|t| t.server.as_ref()) {
            if !servers.contains(s) {
                servers.push(s.clone());
            }
        }
        if servers.is_empty() {
            continue;
        }
        for s in connected {
            if !servers.contains(s) {
                servers.push(s.clone());
            }
        }
        let mut seen: HashSet<String> = window.tabs.iter().map(|t| t.key.clone()).collect();
        for server in servers {
            for t in synthesize(&server, &live(&server), claimed) {
                if seen.insert(t.key.clone()) {
                    window.tabs.push(t);
                }
            }
        }
    }
}

/// [`add_unclaimed`] from the hub's session lists, a session counting as claimed when a pane of
/// one of `config`'s own Rex tabs (main or split) maps to it — so a session whose tab left the
/// config, or moved server, is listed.
pub fn append(config: &mut Config, hub: &RexHub) {
    let pane_keys: Vec<String> = config
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .filter(|t| t.server.is_some() && !is_unclaimed(&t.key, &t.dir, t.server.as_deref()))
        .flat_map(|t| [t.key.clone(), crate::registry::secondary_id(&t.key)])
        .collect();
    let claimed = hub.claimed_session_ids(pane_keys.iter().map(String::as_str));
    add_unclaimed(
        config,
        &hub.servers(),
        |server| hub.live_sessions(server),
        &claimed,
    );
}

/// Bring every window's unclaimed sessions up to date with the hub, from any thread but the main
/// one holding `ManagerState`: on the main thread, re-[`append`] `last_good` (what's on screen) and
/// reconcile the difference, like `rescan_root` without the rescan. A row that opened keeps its
/// terminal; a row whose session ended goes, with its terminal.
pub fn refresh(app: &AppHandle) {
    let main = app.clone();
    let _ = app.run_on_main_thread(move || {
        let (Some(st), Some(hub)) = (main.try_state::<ManagerState>(), main.try_state::<RexHub>())
        else {
            return;
        };
        {
            let mut m = st.lock();
            let mut fresh = m.last_good.clone();
            append(&mut fresh, &hub);
            if fresh == m.last_good {
                return;
            }
            let recon = warden_config::reconcile(&m.last_good, &fresh);
            m.apply(&main, &recon, fresh);
            m.record_all_sessions();
        }
        super::spawn::refresh_presence(&main, None);
    });
}

/// The servers a launch or reload connects to: those `config`'s tabs name, then those only the
/// hub's persisted mappings name — a session warden made for a tab that has since moved or gone
/// still lists as unclaimed after a restart.
pub fn launch_servers(config: &Config, hub: &RexHub) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    for t in config.windows.iter().flat_map(|w| &w.tabs) {
        if let Some(s) = t.server.as_ref().filter(|s| !servers.contains(s)) {
            servers.push(s.clone());
        }
    }
    for s in hub.store_servers() {
        if !servers.contains(&s) {
            servers.push(s);
        }
    }
    servers
}

/// [`RexHub::connect`] to each of `servers` (never starting Rex), calling `connected` for each
/// that answered; one that doesn't is logged and skipped.
fn connect_all(hub: &RexHub, servers: &[String], mut connected: impl FnMut(&str)) {
    for server in servers {
        match hub.connect(server) {
            Ok(()) => connected(server),
            Err(e) => eprintln!("warden: Rex server {server}: {e}"),
        }
    }
}

/// Connect, on a worker, to every Rex server [`launch_servers`] names, then refresh what they
/// list — so a window shows its servers' unclaimed sessions (and its Rex tabs' dots) at launch and
/// after a reload, without waiting for one of its tabs to open.
pub fn connect(app: &AppHandle, config: &Config) {
    let Some(hub) = app.try_state::<RexHub>() else {
        return;
    };
    let servers = launch_servers(config, &hub);
    if servers.is_empty() {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("warden-rex-connect".into())
        .spawn(move || {
            let Some(hub) = app.try_state::<RexHub>() else {
                return;
            };
            connect_all(&hub, &servers, |server| {
                super::spawn::hub_changed(&app, Some(server))
            });
        });
    if let Err(e) = spawned {
        eprintln!("warden: couldn't start connecting to Rex servers: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use warden_config::{Colour, Density, TabDigitKeys, Window};

    fn session(id: &str, label: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            label: label.into(),
        }
    }

    fn tab(key: &str, server: Option<&str>) -> Tab {
        Tab {
            id: None,
            key: key.into(),
            title: key.into(),
            dir: PathBuf::from("/w"),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: None,
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: server.map(Into::into),
            split: None,
        }
    }

    fn window(title: &str, tabs: Vec<Tab>) -> Window {
        Window {
            title: title.into(),
            colour: Colour { r: 0, g: 0, b: 0 },
            width: 1500,
            height: 1000,
            open_on_start: true,
            open_tabs_section: false,
            remember_tabs: false,
            tabs,
            roots: Vec::new(),
        }
    }

    fn config(windows: Vec<Window>) -> Config {
        Config {
            windows,
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
        }
    }

    #[test]
    fn synthesize_lists_only_unclaimed() {
        let live = [
            session("session:mine", "proj · main"),
            session("session:phone", "made on the phone"),
            session("session:blank", ""),
        ];
        let claimed = HashSet::from(["session:mine".to_string()]);
        let tabs = synthesize("local", &live, &claimed);
        let keys: Vec<_> = tabs.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, ["rex:session:phone", "rex:session:blank"]);
        let t = &tabs[0];
        assert_eq!(t.title, "made on the phone");
        assert_eq!(t.group.as_deref(), Some("Rex · local"));
        assert_eq!(t.server.as_deref(), Some("local"));
        assert_eq!(t.dir, PathBuf::new());
        assert_eq!(t.startup, None);
        assert_eq!(t.split, None);
        assert!(!t.load_on_open);
        assert_eq!(
            tabs[1].title, "session:blank",
            "an unlabelled session shows its id"
        );
        assert!(is_unclaimed(&t.key, &t.dir, t.server.as_deref()));
        assert_eq!(session_of(&t.key), Some("session:phone"));
    }

    #[test]
    fn window_without_rex_tabs_shows_no_unclaimed() {
        let mut cfg = config(vec![
            window("rex", vec![tab("plain", None), tab("agent", Some("local"))]),
            window("plain", vec![tab("other", None)]),
        ]);
        let live = |server: &str| match server {
            "local" => vec![session("session:mine", "m"), session("session:orphan", "o")],
            _ => vec![session("session:elsewhere", "e")],
        };
        let claimed = HashSet::from(["session:mine".to_string()]);
        add_unclaimed(&mut cfg, &["local".into()], live, &claimed);
        let keys = |w: &Window| w.tabs.iter().map(|t| t.key.clone()).collect::<Vec<_>>();
        assert_eq!(
            keys(&cfg.windows[0]),
            ["plain", "agent", "rex:session:orphan"]
        );
        assert_eq!(keys(&cfg.windows[1]), ["other"], "no Rex tabs, no section");

        // Run again (a refresh over the result): the old rows go, the current ones come back once.
        add_unclaimed(
            &mut cfg,
            &["local".into()],
            |_| vec![session("session:new", "n")],
            &claimed,
        );
        assert_eq!(keys(&cfg.windows[0]), ["plain", "agent", "rex:session:new"]);
    }

    #[test]
    fn a_window_with_rex_tabs_lists_every_connected_server() {
        // The window's only local tab moved to a remote: its old local session must still show.
        let mut cfg = config(vec![window("rex", vec![tab("agent", Some("remote"))])]);
        let live = |server: &str| match server {
            "local" => vec![session("session:left-behind", "l")],
            _ => vec![session("session:remote-orphan", "r")],
        };
        add_unclaimed(
            &mut cfg,
            &["local".into(), "remote".into()],
            live,
            &HashSet::new(),
        );
        let rows: Vec<_> = cfg.windows[0]
            .tabs
            .iter()
            .map(|t| (t.key.as_str(), t.group.as_deref()))
            .collect();
        assert_eq!(
            rows,
            [
                ("agent", None),
                ("rex:session:remote-orphan", Some("Rex · remote")),
                ("rex:session:left-behind", Some("Rex · local")),
            ],
            "the window's own servers first, then every other connected one"
        );
    }

    #[test]
    fn a_curated_tab_is_never_taken_for_an_unclaimed_one() {
        let curated = tab("rex:mine", Some("local"));
        assert!(!is_unclaimed(
            &curated.key,
            &curated.dir,
            curated.server.as_deref()
        ));
        assert!(!is_unclaimed("rex:s", Path::new(""), None), "not a Rex tab");
    }

    fn test_info() -> rex_client::ClientInfo {
        rex_client::ClientInfo {
            kind: "app",
            name: "warden-test".into(),
            version: "0.0.0".into(),
        }
    }

    fn seed_store(path: &Path, pane: &str, server: &str, session_id: &str) {
        super::super::store::RexStore::load(path.to_path_buf()).set(
            pane,
            Some(super::super::store::PaneRecord {
                server: server.into(),
                session_id: session_id.into(),
            }),
        );
    }

    fn lists(sessions: serde_json::Value) -> rex_client::fake::FakeServer {
        let fake = rex_client::fake::FakeServer::start();
        fake.reply(
            "session.attach_all",
            serde_json::json!({ "sessions": sessions }),
        );
        fake
    }

    #[test]
    fn launch_connects_servers_only_the_store_names() {
        let remote = lists(serde_json::json!([{"session_id": "session:far", "label": "far"}]));
        let local = lists(serde_json::json!([]));
        local.reply(
            "hosts.list",
            serde_json::json!({"hash": "h", "hosts": [
                {"label": "remote-host", "endpoints": [remote.endpoint().to_string()]}
            ]}),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rex-sessions.json");
        seed_store(&path, "moved-tab", "remote-host", "session:far");
        let hub = RexHub::for_test(path, test_info(), local.endpoint());

        let cfg = config(vec![window("w", vec![tab("a", Some("local"))])]);
        let servers = launch_servers(&cfg, &hub);
        assert_eq!(servers, ["local", "remote-host"]);
        connect_all(&hub, &servers, |_| {});

        let mut shown = cfg.clone();
        let claimed = hub.claimed_session_ids(["a"]);
        add_unclaimed(
            &mut shown,
            &hub.servers(),
            |s| hub.live_sessions(s),
            &claimed,
        );
        let keys: Vec<&str> = shown.windows[0]
            .tabs
            .iter()
            .map(|t| t.key.as_str())
            .collect();
        assert!(keys.contains(&"rex:session:far"), "{keys:?}");
    }

    #[test]
    fn launch_connect_with_store_servers_never_starts_rex() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static STARTS: AtomicUsize = AtomicUsize::new(0);
        fn counting(_: std::time::Duration) -> Result<(), rex_client::RexError> {
            STARTS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rex-sessions.json");
        seed_store(&path, "p", "remote-host", "session:far");
        seed_store(&path, "q", "local", "session:x");
        let hub = RexHub::for_test_autostart(
            path,
            test_info(),
            rex_client::Endpoint::Unix(dir.path().join("no-server.sock")),
            counting,
        );
        let servers = launch_servers(&config(vec![window("w", vec![])]), &hub);
        assert_eq!(servers, ["local", "remote-host"]);
        connect_all(&hub, &servers, |_| {});
        assert_eq!(
            STARTS.load(Ordering::SeqCst),
            0,
            "a Rex the user quit stays quit"
        );
    }
}
