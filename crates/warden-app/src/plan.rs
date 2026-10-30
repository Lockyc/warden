//! Pure bridge: `warden_config` types → app-side window/tab descriptors and the
//! [`WindowDecl`]s shell-core's window set takes. No AppKit, no Tauri — fully unit-tested.

use crate::surface::TabSpec;
use shell_core::menu::WindowEntry;
use shell_core::windows::plan::{sanitize_label, WindowDecl};
use std::collections::HashMap;
use std::path::Path;
use warden_config::{tree_path, Colour, Config, Reconciliation, Tab, Window};

/// Derive (tree, tree_path) for a tab from its window's roots (root name → dir).
/// A tab whose `group` names a root is a tree row; tree_path = folder segments
/// between that root's dir and the tab's dir (empty for a project directly under
/// the root). A tab whose group is None or names a plain [[window.group]] → (false, []).
pub fn derive_tree_meta(
    root_dirs: &HashMap<&str, &Path>,
    group: Option<&str>,
    dir: &Path,
) -> (bool, Vec<String>) {
    match group.and_then(|g| root_dirs.get(g)) {
        Some(root_dir) => (true, tree_path(root_dir, dir)),
        None => (false, Vec::new()),
    }
}

/// A tab to materialize, plus its spawn policy. `load_on_open` drives lazy-vs-eager
/// spawn in the registry (spec §3); the surface layer itself never sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TabPlan {
    pub spec: TabSpec,
    pub load_on_open: bool,
}

/// Build a `TabPlan` for one resolved `Tab` given its window's root dirs. The single
/// site that maps a `warden_config::Tab` to the app's `TabPlan`/`TabSpec` — used by
/// `window_to_spec` and by `tab_ops` for both added and respawned tabs.
pub fn tab_to_plan(root_dirs: &HashMap<&str, &Path>, t: &Tab) -> TabPlan {
    let (tree, tree_path) = derive_tree_meta(root_dirs, t.group.as_deref(), &t.dir);
    TabPlan {
        spec: TabSpec {
            id: t.key.clone(),
            title: t.title.clone(),
            dir: t.dir.clone(),
            shell: t.shell.clone(),
            startup: t.startup.clone(),
            group: t.group.clone(),
            probe: t.probe.clone(),
            kill: t.kill.clone(),
            suspend: t.suspend.clone(),
            tree,
            tree_path,
            split: t.split.clone(),
            server: t.server.clone(),
            resume: t.resume.clone(),
        },
        load_on_open: t.load_on_open,
    }
}

/// Everything needed to build one window's content under the label shell-core's window set chose.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub label: String,  // the Tauri window label (shell-core's `WindowSet` assigns it)
    pub title: String,  // window title, verbatim — banner + window title
    pub colour: String, // "#rrggbb" from Colour::hex()
    pub width: f64,     // inner width in logical pixels
    pub height: f64,    // inner height in logical pixels
    pub open_tabs_section: bool, // this window's sidebar pins an "Open" section (cascade collapsed)
    pub remember_tabs: bool, // restore this window's remembered loaded + active tabs at build
    pub tabs: Vec<TabPlan>,
}

/// The opaque token identifying a popped-out tab's detached window, built from its origin
/// window label + the tab's id (= `Tab::key`). Combined and sanitized to the Tauri label
/// charset, then suffixed with a hash of the raw pair, so `shell_core::detach::detached_label(token)`
/// is always a valid, distinct label: two different tabs (different id) never collide — not even
/// ids that sanitize alike (`/x/my proj` and `/x/my-proj`) — and the same tab can't be popped out twice
/// (its registry slot goes `Detached` on the first pop-out, so a second `detach` is a no-op).
/// The value is never parsed back — the manager tracks `origin_label` + `tab_id` on the
/// `DetachedSurface` itself — so the colon separator is purely for human legibility of the label.
pub fn detach_window_token(origin_label: &str, tab_id: &str) -> String {
    let raw = format!("{origin_label}:{tab_id}");
    // FNV-1a: stable across builds and Rust versions, since the label also keys the pop-out's
    // persisted geometry.
    let hash = raw.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    format!("{}-{:08x}", sanitize_label(&raw), hash as u32)
}

/// Build a `WindowSpec` for one window under an already-chosen `label`.
/// Tab id = `Tab::key` — the reconcile identity: the `id`-else-normalized-`dir`
/// for a curated tab, or the absolute project path for a discovered (`[[window.root]]`) tab.
pub fn window_to_spec(p: &Window, label: String) -> WindowSpec {
    let root_dirs: HashMap<&str, &Path> = p
        .roots
        .iter()
        .map(|r| (r.name.as_str(), r.dir.as_path()))
        .collect();
    let tabs = p.tabs.iter().map(|t| tab_to_plan(&root_dirs, t)).collect();
    WindowSpec {
        label,
        title: p.title.clone(),
        colour: p.colour.hex(),
        width: p.width as f64,
        height: p.height as f64,
        open_tabs_section: p.open_tabs_section,
        remember_tabs: p.remember_tabs,
        tabs,
    }
}

/// What shell-core's window set needs to know about one `[[window]]`. The colour is always the
/// resolved one (an omitted `colour` is warden's neutral default), so the window form and the home
/// surface show the accent the banner paints.
pub fn decl(w: &Window) -> WindowDecl {
    WindowDecl {
        title: w.title.clone(),
        colour: Some(w.colour.hex()),
        width: w.width as f64,
        height: w.height as f64,
        open_on_start: w.open_on_start,
    }
}

/// Every window's [`decl`], config order.
pub fn decls(config: &Config) -> Vec<WindowDecl> {
    config.windows.iter().map(decl).collect()
}

/// Carry a window's in-place title or colour change into a held config ahead of the reload that
/// will echo it: patch the window titled `title` (a colour change), else the one window whose
/// title `listed` no longer accepts (a rename). With several unlisted (another edit in flight)
/// nothing is patched and the reload settles it.
pub fn retitle_window(
    config: &mut Config,
    title: &str,
    colour: Colour,
    listed: impl Fn(&str) -> bool,
) {
    let title = title.trim();
    let idx = config
        .windows
        .iter()
        .position(|w| w.title == title)
        .or_else(|| {
            let mut unlisted = config
                .windows
                .iter()
                .enumerate()
                .filter(|(_, w)| !listed(&w.title))
                .map(|(i, _)| i);
            match (unlisted.next(), unlisted.next()) {
                (Some(i), None) => Some(i),
                _ => None,
            }
        });
    if let Some(w) = idx.map(|i| &mut config.windows[i]) {
        w.title = title.to_string();
        w.colour = colour;
    }
}

/// The label `⌘⇧T` should reopen: the most-recently-closed title (top of `last_closed`) that the
/// window set still lists closed. Skips titles since deleted from the config or reopened.
pub fn next_reopen_target(last_closed: &[String], entries: &[WindowEntry]) -> Option<String> {
    last_closed.iter().rev().find_map(|title| {
        entries
            .iter()
            .find(|e| !e.open && e.title == *title)
            .map(|e| e.id.clone())
    })
}

/// A hot-reload's changes to one kept window's tabs. Opening, closing and recolouring windows is
/// shell-core's window set's; this is the part only warden knows.
#[derive(Debug, Clone, PartialEq)]
pub struct TabOp {
    pub label: String,
    // The window's "Open" section flag flipped — chrome-only, no surface touched.
    pub open_tabs_section: Option<bool>,
    pub add_tabs: Vec<TabPlan>,
    pub remove_tabs: Vec<String>, // tab ids (= Tab::key)
    pub order: Vec<String>,       // full new tab id order
    // In-place metadata for kept tabs whose group/probe/kill changed —
    // applied live (sidebar re-section + new probe/kill) without respawning.
    pub set_meta: Vec<MetaPlan>,
    // Kept tabs whose terminal spec changed — respawn in place (remove+add by id).
    pub respawn_tabs: Vec<TabPlan>,
    // Kept tab ids whose load_on_open turned on — spawn if cold, never respawn.
    pub spawn_tabs: Vec<String>,
}

/// A kept tab's in-place metadata for a hot-reload, ready to apply to a live tab. Wraps
/// the crate's `TabMeta` (title/group/probe/kill) and adds the app-side `tree`/`tree_path`
/// **recomputed from the new config's roots**. This recompute is load-bearing: a kept tab
/// (same `Tab::key`) can flip between a project-tree (root) section and a plain group when a
/// curated tab shadows — or stops shadowing — a same-dir discovered project (its normalized
/// dir key equals the discovery's path key, so `reconcile` sees a kept tab with a changed
/// `group`, i.e. `set_meta`, not add/remove). `TabMeta` is crate-pure and can't carry
/// tree-ness, so it's derived here alongside the `add_tabs`/`respawn_tabs` derivation.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaPlan {
    pub id: String,
    pub meta: warden_config::TabMeta,
    pub tree: bool,
    pub tree_path: Vec<String>,
}

/// Map a reconciliation's kept-window updates (by title) to tab ops (by the label `label_of`
/// gives the title). A title with no label is skipped.
pub fn tab_ops(
    recon: &Reconciliation,
    new_config: &Config,
    label_of: impl Fn(&str) -> Option<String>,
) -> Vec<TabOp> {
    recon
        .update
        .iter()
        .filter_map(|u| {
            let label = label_of(&u.title)?;
            let win = new_config.windows.iter().find(|w| w.title == u.title);
            let root_dirs: HashMap<&str, &Path> = win
                .map(|w| {
                    w.roots
                        .iter()
                        .map(|r| (r.name.as_str(), r.dir.as_path()))
                        .collect()
                })
                .unwrap_or_default();
            let plans = |tabs: &[Tab]| tabs.iter().map(|t| tab_to_plan(&root_dirs, t)).collect();
            // Recompute each kept tab's tree/tree_path from the NEW config's roots + its
            // (new) group — the same derivation `tab_to_plan` runs for adds/respawns. A
            // shadow flip (curated tab appears/disappears at a discovered project's dir)
            // is a set_meta group change on a kept key, so its tree-ness must follow the
            // new group, not linger from the old one.
            let set_meta = u
                .set_meta
                .iter()
                .map(|(id, meta)| {
                    let (tree, tree_path) = win
                        .and_then(|w| w.tabs.iter().find(|t| &t.key == id))
                        .map(|t| derive_tree_meta(&root_dirs, meta.group.as_deref(), &t.dir))
                        .unwrap_or((false, Vec::new()));
                    MetaPlan {
                        id: id.clone(),
                        meta: meta.clone(),
                        tree,
                        tree_path,
                    }
                })
                .collect();
            Some(TabOp {
                label,
                open_tabs_section: u.open_tabs_section,
                add_tabs: plans(&u.add_tabs),
                remove_tabs: u.remove_tabs.clone(),
                order: u.tab_order.clone(),
                set_meta,
                respawn_tabs: plans(&u.respawn_tabs),
                spawn_tabs: u.spawn_tabs.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_config::{load, reconcile, Config};

    fn cfg(toml: &str) -> Config {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        f.sync_all().unwrap();
        load(&path).unwrap().config
    }

    /// The one window's spec under its own title as label — the tab-mapping tests' entry point.
    fn spec_of(c: &Config, i: usize) -> WindowSpec {
        window_to_spec(&c.windows[i], c.windows[i].title.clone())
    }

    /// Every title labelled by itself — what the window set gives windows with label-safe titles.
    fn by_title(t: &str) -> Option<String> {
        Some(t.to_string())
    }

    fn entry(id: &str, title: &str, open: bool) -> WindowEntry {
        WindowEntry {
            id: id.into(),
            title: title.into(),
            open,
            colour: None,
        }
    }

    #[test]
    fn decls_carry_title_size_open_on_start_and_the_resolved_colour() {
        let c = cfg(r##"
[[window]]
title = "a"
open_on_start = false
width = 800
height = 600
colour = "#112233"
[[window]]
title = "b"
"##);
        let d = decls(&c);
        assert_eq!(
            d[0],
            WindowDecl {
                title: "a".into(),
                colour: Some("#112233".into()),
                width: 800.0,
                height: 600.0,
                open_on_start: false,
            }
        );
        assert!(d[1].open_on_start, "default true carries through");
        assert_eq!(
            d[1].colour.as_deref(),
            Some(warden_config::resolve::DEFAULT_COLOUR.hex().as_str()),
            "an omitted colour is the neutral default the banner paints"
        );
    }

    #[test]
    fn retitle_window_patches_the_renamed_or_recoloured_window() {
        let mut c = cfg("[[window]]\ntitle = \"Docs\"\n[[window]]\ntitle = \"Side\"\n");
        let red = Colour::parse("#ff0000").unwrap();
        // "Docs" was renamed to "Notes": "Docs" is the one window no longer listed.
        let listed = |t: &str| ["Notes", "Side"].contains(&t);
        retitle_window(&mut c, "Notes", red, listed);
        assert_eq!(c.windows[0].title, "Notes");
        assert_eq!(c.windows[0].colour, red);
        // A colour change patches the window by its own title.
        retitle_window(&mut c, " Side ", red, listed);
        assert_eq!(c.windows[1].title, "Side");
        assert_eq!(c.windows[1].colour, red);
        // Two unlisted windows (another edit in flight): nothing is patched.
        retitle_window(&mut c, "Other", red, |t| t == "Other");
        let titles: Vec<&str> = c.windows.iter().map(|w| w.title.as_str()).collect();
        assert_eq!(titles, ["Notes", "Side"]);
    }

    #[test]
    fn reopen_target_none_when_stack_empty() {
        assert_eq!(
            next_reopen_target(&[], &[entry("work", "work", false)]),
            None
        );
    }

    #[test]
    fn reopen_target_picks_the_most_recent_closed_title_and_returns_its_label() {
        // Closed in order: work, then side. side is the most recent; its label differs from
        // its title, and the label is what reopens it.
        let stack = vec!["work".to_string(), "side project".to_string()];
        let entries = [
            entry("work", "work", false),
            entry("side-project", "side project", false),
        ];
        assert_eq!(
            next_reopen_target(&stack, &entries),
            Some("side-project".to_string())
        );
    }

    #[test]
    fn reopen_target_skips_open_and_unconfigured_titles() {
        let stack = vec!["work".to_string(), "gone".to_string(), "side".to_string()];
        // side is back open and gone left the config, so the next reopenable is work.
        let entries = [entry("work", "work", false), entry("side", "side", true)];
        assert_eq!(
            next_reopen_target(&stack, &entries),
            Some("work".to_string())
        );
        let all_open = [entry("work", "work", true), entry("side", "side", true)];
        assert_eq!(next_reopen_target(&stack, &all_open), None);
    }

    #[test]
    fn detach_window_token_is_label_safe_and_distinct_per_tab() {
        // Two different tabs from the same origin get distinct tokens (so their detached
        // windows never collide on one label).
        let t1 = detach_window_token("work", "/tmp/alpha");
        let t2 = detach_window_token("work", "/tmp/beta");
        assert_ne!(t1, t2);

        // Round-trips into a valid, recognizable detached-window label.
        let label = shell_core::detach::detached_label(&t1);
        assert!(shell_core::detach::is_detached_label(&label));

        // An id carrying spaces/unicode is sanitized to the Tauri label charset, so the
        // resulting label is always valid.
        let t3 = detach_window_token("work", "my project ☕");
        assert!(
            t3.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | ':' | '_')),
            "token must be restricted to the Tauri label charset, got {t3:?}"
        );

        // Ids that differ only in a disallowed character sanitize alike; the hash keeps them apart.
        assert_ne!(
            detach_window_token("work", "/x/my proj"),
            detach_window_token("work", "/x/my-proj")
        );
        assert_ne!(
            detach_window_token("work", "/x/é"),
            detach_window_token("work", "/x/ü")
        );
        // Deterministic: the same tab always gets the same label (and so its saved geometry).
        assert_eq!(t1, detach_window_token("work", "/tmp/alpha"));
    }

    #[test]
    fn window_to_spec_maps_window_and_tabs() {
        let c = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  load_on_open = true
  [[window.tab]]
  title = "ops"
  dir = "/tmp/ops"
"##);
        let w = spec_of(&c, 0);
        assert_eq!(w.label, "work");
        assert_eq!(w.title, "work");
        assert_eq!(w.colour, "#0f8a8a");
        assert_eq!(w.tabs.len(), 2);
        // No explicit `id` set on either tab, so key (and thus TabSpec.id) is the
        // normalized dir, not the title (Task 1: id-else-dir identity).
        assert_eq!(w.tabs[0].spec.id, "/tmp/alpha");
        assert_eq!(w.tabs[0].spec.title, "alpha");
        assert!(w.tabs[0].load_on_open);
        assert_eq!(w.tabs[1].spec.id, "/tmp/ops");
        assert!(!w.tabs[1].load_on_open);
    }

    #[test]
    fn tree_tabs_get_tree_flag_and_relative_path() {
        use warden_config::{Root, Tab, Window};
        let root_dir = std::path::PathBuf::from("/r/Dev");
        let win = Window {
            title: "dev".into(),
            colour: Colour { r: 0, g: 0, b: 0 },
            width: 1500,
            height: 1000,
            open_on_start: true,
            open_tabs_section: false,
            remember_tabs: false,
            tabs: vec![Tab {
                id: None,
                key: "/r/Dev/gh/lockyc/warden".into(),
                title: "warden".into(),
                dir: "/r/Dev/gh/lockyc/warden".into(),
                shell: "sh".into(),
                startup: None,
                load_on_open: false,
                group: Some("Dev".into()),
                probe: None,
                kill: None,
                suspend: None,
                resume: None,
                server: None,
                split: None,
            }],
            roots: vec![Root {
                name: "Dev".into(),
                dir: root_dir,
                depth: 6,
                shell: "sh".into(),
                startup: None,
                probe: None,
                kill: None,
                suspend: None,
                resume: None,
                server: None,
                split: None,
            }],
        };
        let spec = window_to_spec(&win, "dev".into());
        assert_eq!(
            spec.tabs[0].spec.tree_path,
            vec!["gh".to_string(), "lockyc".to_string()]
        );
        assert!(spec.tabs[0].spec.tree);
    }

    #[test]
    fn tab_ops_cover_kept_windows_only_under_their_labels() {
        let base = r##"
[[window]]
title = "work zone"
  [[window.tab]]
  dir = "/tmp/alpha"
"##;
        let old = cfg(base);
        let new = cfg(&format!(
            "{base}  [[window.tab]]\n  dir = \"/tmp/beta\"\n[[window]]\ntitle = \"new\"\n"
        ));
        let r = reconcile(&old, &new);
        let ops = tab_ops(&r, &new, |t| (t == "work zone").then(|| "work-zone".into()));
        assert_eq!(
            ops.len(),
            1,
            "the opened window is the window set's: {ops:?}"
        );
        assert_eq!(ops[0].label, "work-zone");
        assert_eq!(ops[0].add_tabs[0].spec.id, "/tmp/beta");
        // A title the window set doesn't list is skipped.
        assert!(tab_ops(&r, &new, |_| None).is_empty());
    }

    #[test]
    fn window_to_spec_carries_kill_onto_spec() {
        let c = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
kill = "kill-session"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  [[window.tab]]
  title = "ops"
  dir = "/tmp/ops"
  kill = ""
"##);
        let spec = spec_of(&c, 0);
        assert_eq!(spec.tabs[0].spec.kill.as_deref(), Some("kill-session"));
        assert_eq!(spec.tabs[1].spec.kill, None); // opted out
    }

    #[test]
    fn window_to_spec_carries_probe_onto_spec() {
        let c = cfg(r##"
probe = "check-session"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  [[window.tab]]
  title = "ops"
  dir = "/tmp/ops"
  probe = ""
"##);
        let spec = spec_of(&c, 0);
        assert_eq!(spec.tabs[0].spec.probe.as_deref(), Some("check-session"));
        assert_eq!(spec.tabs[1].spec.probe, None); // opted out
    }

    /// A tab added by a hot-reload reconcile (`WindowUpdate.add_tabs`) gets the same
    /// tree/tree_path derivation as the initial build path (`window_to_spec`), so a
    /// project discovered by a rescan/hot-reload nests under its tree root, not loose.
    #[test]
    fn reconcile_add_tab_gets_tree_meta_from_new_config_roots() {
        use warden_config::{Density, Root, Tab, TabDigitKeys, WindowUpdate};

        let added = Tab {
            id: None,
            key: "/r/Dev/gh/lockyc/warden".into(),
            title: "warden".into(),
            dir: "/r/Dev/gh/lockyc/warden".into(),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: Some("Dev".into()),
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
        };
        let recon = Reconciliation {
            open: Vec::new(),
            close: Vec::new(),
            update: vec![WindowUpdate {
                title: "dev".into(),
                colour: None,
                open_tabs_section: None,
                add_tabs: vec![added],
                remove_tabs: Vec::new(),
                tab_order: vec!["/r/Dev/gh/lockyc/warden".into()],
                set_meta: Vec::new(),
                respawn_tabs: Vec::new(),
                spawn_tabs: Vec::new(),
            }],
        };
        let new_config = Config {
            windows: vec![Window {
                title: "dev".into(),
                colour: Colour { r: 0, g: 0, b: 0 },
                width: 1500,
                height: 1000,
                open_on_start: true,
                open_tabs_section: false,
                remember_tabs: false,
                tabs: Vec::new(),
                roots: vec![Root {
                    name: "Dev".into(),
                    dir: "/r/Dev".into(),
                    depth: 6,
                    shell: "sh".into(),
                    startup: None,
                    probe: None,
                    kill: None,
                    suspend: None,
                    resume: None,
                    server: None,
                    split: None,
                }],
            }],
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
        };

        let ops = tab_ops(&recon, &new_config, by_title);
        assert_eq!(ops.len(), 1);
        let add_tabs = &ops[0].add_tabs;
        assert_eq!(add_tabs.len(), 1);
        assert!(add_tabs[0].spec.tree, "added tree tab must be flagged tree");
        assert_eq!(
            add_tabs[0].spec.tree_path,
            vec!["gh".to_string(), "lockyc".to_string()]
        );
    }

    /// A `[[window.root]]`-discovered project (tree row, `group` = root name)
    /// that a curated tab then shadows at the same `dir` (moving it into a plain group) is a
    /// KEPT tab — same `Tab::key` — so reconcile emits a `set_meta` group change, not add/
    /// remove. Its `tree`/`tree_path` must be recomputed from the new group and cleared;
    /// leaving them stale rendered the shadowed project as its own tree section under the
    /// curated group's name (a "TOOLS" tree row instead of a plain TOOLS group tab).
    #[test]
    fn reconcile_shadow_transition_clears_stale_tree_meta_via_set_meta() {
        use warden_config::{Density, Root, Tab, TabDigitKeys};

        let key = "/r/Dev/gh/lockyc/mycelium".to_string();
        let root = Root {
            name: "Developer".into(),
            dir: "/r/Dev".into(),
            depth: 6,
            shell: "sh".into(),
            startup: None,
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
        };
        let mk_window = |tabs: Vec<Tab>| Window {
            title: "w".into(),
            colour: Colour { r: 0, g: 0, b: 0 },
            width: 1500,
            height: 1000,
            open_on_start: true,
            open_tabs_section: false,
            remember_tabs: false,
            tabs,
            roots: vec![root.clone()],
        };
        let mk_cfg = |tabs: Vec<Tab>| Config {
            windows: vec![mk_window(tabs)],
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
        };

        // OLD: the project is only discovered by the root → a tree row under "Developer".
        let discovered = Tab {
            id: None,
            key: key.clone(),
            title: "mycelium".into(),
            dir: key.clone().into(),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: Some("Developer".into()),
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
        };
        let old = mk_cfg(vec![discovered]);

        // NEW: a curated tab in the "tools" group at the SAME dir shadows the discovery
        // (same key). Its group is a plain [[window.group]], not a root → not a tree row.
        let curated = Tab {
            id: None,
            key: key.clone(),
            title: "mycelium".into(),
            dir: key.clone().into(),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: Some("tools".into()),
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
        };
        let new = mk_cfg(vec![curated]);

        let recon = warden_config::reconcile(&old, &new);
        // Same key + changed group ⇒ a kept-tab metadata update, not add/remove.
        assert_eq!(recon.update.len(), 1, "expected one window update");
        assert!(
            recon.update[0].add_tabs.is_empty() && recon.update[0].remove_tabs.is_empty(),
            "shadow transition must be a kept tab, not add/remove"
        );

        let ops = tab_ops(&recon, &new, by_title);
        let m = ops[0]
            .set_meta
            .iter()
            .find(|m| m.id == key)
            .expect("set_meta must carry the shadowed tab");
        assert_eq!(m.meta.group.as_deref(), Some("tools"));
        assert!(
            !m.tree,
            "shadowed project is now a plain group tab — tree must be cleared"
        );
        assert!(
            m.tree_path.is_empty(),
            "cleared tree row must have no tree_path"
        );
    }

    /// A kept tab identified by an explicit `id` whose `dir` changes must respawn
    /// (remove+add by id under the surface), carried via `WindowUpdate.respawn_tabs`
    /// — not treated as a plain add/remove (which is what happens when identity is
    /// dir-derived and the dir itself changes).
    #[test]
    fn respawn_tab_maps_to_tabplan_with_new_dir() {
        let old = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  id = "a"
  dir = "/tmp/a"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  id = "a"
  dir = "/tmp/a-new"
"##);
        let r = reconcile(&old, &new);
        let ops = tab_ops(&r, &new, by_title);
        assert_eq!(ops.len(), 1);
        let respawn_tabs = &ops[0].respawn_tabs;
        assert_eq!(respawn_tabs.len(), 1);
        assert_eq!(respawn_tabs[0].spec.id, "a");
        assert_eq!(
            respawn_tabs[0].spec.dir,
            std::path::PathBuf::from("/tmp/a-new")
        );
    }
}
