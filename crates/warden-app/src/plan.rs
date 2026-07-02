//! Pure bridge: `warden_config` types → app-side window/tab descriptors, plus
//! Tauri window-label sanitization. No AppKit, no Tauri — fully unit-tested.

use crate::manager::DIAG_LABEL;
use crate::surface::TabSpec;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use warden_config::{Config, Reconciliation, Window};

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
        Some(root_dir) => (true, crate::scanner::tree_path(root_dir, dir)),
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

/// Everything needed to build one window window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub label: String,  // sanitized, unique — the Tauri window label
    pub title: String,  // window title, verbatim — banner + window title
    pub colour: String, // "#rrggbb" from Colour::hex()
    pub width: f64,     // inner width in logical pixels
    pub height: f64,    // inner height in logical pixels
    pub tabs: Vec<TabPlan>,
}

/// Map an arbitrary window title to the Tauri label charset `[A-Za-z0-9-/:_]`.
/// Disallowed chars → '-'; leading/trailing '-' trimmed; empty → "window".
pub fn sanitize_label(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | ':' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.starts_with('-') {
        s.remove(0);
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        s = "window".to_string();
    }
    s
}

/// Sanitize `name`, then suffix `-2`, `-3`, … until the label is not in `taken`.
pub fn unique_label(name: &str, taken: &HashSet<String>) -> String {
    let base = sanitize_label(name);
    if !taken.contains(&base) {
        return base;
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}-{n}");
        if !taken.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// Build a `WindowSpec` for one window under an already-chosen `label`.
/// Tab id = `Tab::key` — the reconcile identity: the resolved title for a curated
/// tab, or the absolute project path for a discovered (`[[window.root]]`) tab.
pub fn window_to_spec(p: &Window, label: String) -> WindowSpec {
    let root_dirs: HashMap<&str, &Path> = p
        .roots
        .iter()
        .map(|r| (r.name.as_str(), r.dir.as_path()))
        .collect();
    let tabs = p
        .tabs
        .iter()
        .map(|t| {
            let (tree, tree_path) = derive_tree_meta(&root_dirs, t.group.as_deref(), &t.dir);
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
                    tree,
                    tree_path,
                },
                load_on_open: t.load_on_open,
            }
        })
        .collect();
    WindowSpec {
        label,
        title: p.title.clone(),
        colour: p.colour.hex(),
        width: p.width as f64,
        height: p.height as f64,
        tabs,
    }
}

/// Map a whole config to window specs, assigning unique labels in window order.
pub fn window_specs(config: &Config) -> Vec<WindowSpec> {
    // Reserve the diagnostic window's label so a config window whose title
    // sanitizes to it (e.g. "warden diagnostic") gets `-2`, not a collision that
    // silently breaks window-state persistence / crashes config recovery.
    let mut taken = HashSet::new();
    taken.insert(DIAG_LABEL.to_string());
    config
        .windows
        .iter()
        .map(|p| {
            let label = unique_label(&p.title, &taken);
            taken.insert(label.clone());
            window_to_spec(p, label)
        })
        .collect()
}

/// Specs for every configured window (config order) with labels **consistent with
/// the live window set** — the source of truth the Window menu and reopen paths use.
///
/// An open window (its title present in `live_names`) keeps its **actual live label**:
/// a live Tauri window can't be relabeled, so the mapping must match it. A closed
/// window gets a deterministic fresh label via `unique_label`, avoiding every live
/// label (`live_labels`) ∪ the diagnostic reservation ∪ labels assigned earlier here.
///
/// This exists because recomputing labels purely from config order (`window_specs`)
/// diverges from a live window's label whenever two titles sanitize to the same base
/// and the colliding pair was introduced in an order that made `reconcile_ops` (which
/// seeds from live labels) suffix the *other* one — which made the Window menu raise
/// the wrong window, reopen rebuild a duplicate, and `⌘⇧T` miss the closed window.
pub fn configured_specs(
    config: &Config,
    live_names: &HashMap<String, String>,
    live_labels: &HashSet<String>,
) -> Vec<WindowSpec> {
    let mut taken: HashSet<String> = live_labels.clone();
    taken.insert(DIAG_LABEL.to_string());
    config
        .windows
        .iter()
        .map(|w| {
            let label = match live_names.get(&w.title) {
                Some(live) => live.clone(),
                None => {
                    let l = unique_label(&w.title, &taken);
                    taken.insert(l.clone());
                    l
                }
            };
            window_to_spec(w, label)
        })
        .collect()
}

/// One row of the Window menu: a configured window and whether it is currently open.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowMenuEntry {
    pub label: String,
    pub title: String,
    pub open: bool,
}

/// Map the configured window specs (config order) to menu entries, tagging each
/// with whether its label is currently in the live `open` set.
pub fn window_menu_entries(specs: &[WindowSpec], open: &HashSet<String>) -> Vec<WindowMenuEntry> {
    specs
        .iter()
        .map(|s| WindowMenuEntry {
            label: s.label.clone(),
            title: s.title.clone(),
            open: open.contains(&s.label),
        })
        .collect()
}

/// The label `⌘⇧T` should reopen: the most-recently-closed window (top of the
/// stack) that is still configured and not already open. Skips entries that were
/// closed-then-deleted-from-config or have since been reopened. `None` if none qualify.
pub fn next_reopen_target(
    last_closed: &[String],
    configured: &HashSet<String>,
    open: &HashSet<String>,
) -> Option<String> {
    last_closed
        .iter()
        .rev()
        .find(|l| configured.contains(*l) && !open.contains(*l))
        .cloned()
}

/// One operation to bring the live window set in line with a reloaded config.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowOp {
    Open(WindowSpec),
    Close(String), // label
    Update {
        label: String,
        colour: Option<String>, // new "#rrggbb" if changed
        add_tabs: Vec<TabPlan>,
        remove_tabs: Vec<String>, // tab ids (= Tab::key)
        order: Vec<String>,       // full new tab id order
        // In-place metadata for kept tabs whose group/probe/kill changed —
        // applied live (sidebar re-section + new probe/kill) without respawning.
        set_meta: Vec<(String, warden_config::TabMeta)>,
    },
}

/// Map a reconciliation (by window name) to window ops (by Tauri label).
/// New windows get fresh unique labels avoiding `taken` ∪ labels already
/// assigned earlier in this same call.
pub fn reconcile_ops(
    recon: &Reconciliation,
    new_config: &Config,
    name_to_label: &HashMap<String, String>,
    taken: &HashSet<String>,
) -> Vec<WindowOp> {
    let mut ops = Vec::new();
    let mut assigned: HashSet<String> = taken.clone();
    // Same reservation as window_specs: a newly-opened window must never grab
    // the diagnostic label.
    assigned.insert(DIAG_LABEL.to_string());

    for window in &recon.open {
        let label = unique_label(&window.title, &assigned);
        assigned.insert(label.clone());
        ops.push(WindowOp::Open(window_to_spec(window, label)));
    }

    for name in &recon.close {
        if let Some(label) = name_to_label.get(name) {
            ops.push(WindowOp::Close(label.clone()));
        }
    }

    for u in &recon.update {
        let Some(label) = name_to_label.get(&u.title) else {
            continue;
        };
        let root_dirs: HashMap<&str, &Path> = new_config
            .windows
            .iter()
            .find(|w| w.title == u.title)
            .map(|w| {
                w.roots
                    .iter()
                    .map(|r| (r.name.as_str(), r.dir.as_path()))
                    .collect()
            })
            .unwrap_or_default();
        let add_tabs = u
            .add_tabs
            .iter()
            .map(|t| {
                let (tree, tree_path) = derive_tree_meta(&root_dirs, t.group.as_deref(), &t.dir);
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
                        tree,
                        tree_path,
                    },
                    load_on_open: t.load_on_open,
                }
            })
            .collect();
        ops.push(WindowOp::Update {
            label: label.clone(),
            colour: u.colour.map(|c| c.hex()),
            add_tabs,
            remove_tabs: u.remove_tabs.clone(),
            order: u.tab_order.clone(),
            set_meta: u.set_meta.clone(),
        });
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_config::{load, Config};

    fn set(items: &[&str]) -> std::collections::HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn spec(label: &str, title: &str) -> WindowSpec {
        WindowSpec {
            label: label.to_string(),
            title: title.to_string(),
            colour: "#000000".to_string(),
            width: 800.0,
            height: 600.0,
            tabs: Vec::new(),
        }
    }

    #[test]
    fn menu_entries_preserve_order_and_tag_open_state() {
        let specs = vec![spec("work", "work"), spec("side", "side-project")];
        let open = set(&["work"]);
        let entries = window_menu_entries(&specs, &open);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "work");
        assert_eq!(entries[0].title, "work");
        assert!(entries[0].open);
        assert_eq!(entries[1].label, "side");
        assert!(!entries[1].open);
    }

    #[test]
    fn reopen_target_none_when_stack_empty() {
        assert_eq!(next_reopen_target(&[], &set(&["work"]), &set(&[])), None);
    }

    #[test]
    fn reopen_target_picks_most_recent_closed_configured() {
        // Closed in order: work, then side. side is the most recent.
        let stack = vec!["work".to_string(), "side".to_string()];
        let configured = set(&["work", "side"]);
        let open = set(&[]);
        assert_eq!(
            next_reopen_target(&stack, &configured, &open),
            Some("side".to_string())
        );
    }

    #[test]
    fn reopen_target_skips_already_open() {
        // side is back open, so the next reopenable is work.
        let stack = vec!["work".to_string(), "side".to_string()];
        let configured = set(&["work", "side"]);
        let open = set(&["side"]);
        assert_eq!(
            next_reopen_target(&stack, &configured, &open),
            Some("work".to_string())
        );
    }

    #[test]
    fn reopen_target_skips_no_longer_configured() {
        // side was closed then deleted from config; fall back to work.
        let stack = vec!["work".to_string(), "side".to_string()];
        let configured = set(&["work"]);
        let open = set(&[]);
        assert_eq!(
            next_reopen_target(&stack, &configured, &open),
            Some("work".to_string())
        );
    }

    #[test]
    fn reopen_target_none_when_all_open_or_unconfigured() {
        let stack = vec!["work".to_string(), "side".to_string()];
        let configured = set(&["work", "side"]);
        let open = set(&["work", "side"]);
        assert_eq!(next_reopen_target(&stack, &configured, &open), None);
    }

    fn cfg(toml: &str) -> Config {
        // Reuse the crate's parse+resolve via a temp file load would be heavy;
        // instead go through the public resolve path used in warden-config tests.
        // Simplest: write a tiny helper using the raw+resolve modules is not public,
        // so parse here through `load` on a temp file.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        f.sync_all().unwrap();
        let loaded = load(&path).unwrap();
        // keep tempdir alive until after load
        drop(f);
        let c = loaded.config;
        drop(dir);
        c
    }

    #[test]
    fn sanitizes_spaces_and_unicode() {
        assert_eq!(sanitize_label("work stuff"), "work-stuff");
        assert_eq!(sanitize_label("café ☕"), "caf");
        assert_eq!(sanitize_label("--x--"), "x");
        assert_eq!(sanitize_label("☕☕"), "window");
    }

    #[test]
    fn unique_label_suffixes_on_collision() {
        let mut taken = HashSet::new();
        taken.insert("work".to_string());
        taken.insert("work-2".to_string());
        assert_eq!(unique_label("work", &taken), "work-3");
    }

    #[test]
    fn window_specs_maps_window_and_tabs() {
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
        let specs = window_specs(&c);
        assert_eq!(specs.len(), 1);
        let w = &specs[0];
        assert_eq!(w.label, "work");
        assert_eq!(w.title, "work");
        assert_eq!(w.colour, "#0f8a8a");
        assert_eq!(w.tabs.len(), 2);
        assert_eq!(w.tabs[0].spec.id, "alpha");
        assert_eq!(w.tabs[0].spec.title, "alpha");
        assert!(w.tabs[0].load_on_open);
        assert_eq!(w.tabs[1].spec.id, "ops");
        assert!(!w.tabs[1].load_on_open);
    }

    #[test]
    fn tree_tabs_get_tree_flag_and_relative_path() {
        use warden_config::{Colour, Root, Tab, Window};
        let root_dir = std::path::PathBuf::from("/r/Dev");
        let win = Window {
            title: "dev".into(),
            colour: Colour { r: 0, g: 0, b: 0 },
            width: 1500,
            height: 1000,
            tabs: vec![Tab {
                key: "/r/Dev/gh/lockyc/warden".into(),
                title: "warden".into(),
                dir: "/r/Dev/gh/lockyc/warden".into(),
                shell: "sh".into(),
                startup: None,
                load_on_open: false,
                group: Some("Dev".into()),
                probe: None,
                kill: None,
            }],
            roots: vec![Root {
                name: "Dev".into(),
                dir: root_dir,
                depth: 6,
                shell: "sh".into(),
                startup: None,
                probe: None,
                kill: None,
            }],
        };
        let spec = window_to_spec(&win, "dev".into());
        assert_eq!(
            spec.tabs[0].spec.tree_path,
            vec!["gh".to_string(), "lockyc".to_string()]
        );
        assert!(spec.tabs[0].spec.tree);
    }

    use warden_config::reconcile;

    fn name_label_map(c: &Config) -> HashMap<String, String> {
        window_specs(c)
            .into_iter()
            .map(|w| (w.title, w.label))
            .collect()
    }
    fn taken(c: &Config) -> HashSet<String> {
        window_specs(c).into_iter().map(|w| w.label).collect()
    }

    #[test]
    fn open_window_becomes_open_op() {
        let old = cfg("");
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
"##);
        let r = reconcile(&old, &new);
        let ops = reconcile_ops(&r, &new, &name_label_map(&old), &taken(&old));
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            WindowOp::Open(spec) => assert_eq!(spec.title, "work"),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn closed_window_becomes_close_op_with_label() {
        let old = cfg(r##"
[[window]]
title = "work zone"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
"##);
        let new = cfg("");
        let r = reconcile(&old, &new);
        let ops = reconcile_ops(&r, &new, &name_label_map(&old), &taken(&old));
        assert_eq!(ops, vec![WindowOp::Close("work-zone".to_string())]);
    }

    #[test]
    fn colour_change_becomes_update_op_with_hex() {
        let base = r##"
[[window]]
title = "work"
colour = "{C}"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
"##;
        let old = cfg(&base.replace("{C}", "#0f8a8a"));
        let new = cfg(&base.replace("{C}", "#112233"));
        let r = reconcile(&old, &new);
        let ops = reconcile_ops(&r, &new, &name_label_map(&old), &taken(&old));
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            WindowOp::Update { label, colour, .. } => {
                assert_eq!(label, "work");
                assert_eq!(colour.as_deref(), Some("#112233"));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn window_specs_carries_kill_onto_spec() {
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
        let specs = window_specs(&c);
        assert_eq!(specs[0].tabs[0].spec.kill.as_deref(), Some("kill-session"));
        assert_eq!(specs[0].tabs[1].spec.kill, None); // opted out
    }

    #[test]
    fn window_specs_carries_probe_onto_spec() {
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
        let specs = window_specs(&c);
        assert_eq!(
            specs[0].tabs[0].spec.probe.as_deref(),
            Some("check-session")
        );
        assert_eq!(specs[0].tabs[1].spec.probe, None); // opted out
    }

    #[test]
    fn window_specs_dedupes_labels_for_colliding_sanitized_names() {
        let c = cfg(r##"
[[window]]
title = "a b"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
[[window]]
title = "a-b"
colour = "#222222"
  [[window.tab]]
  title = "t2"
  dir = "/tmp/t2"
"##);
        let specs = window_specs(&c);
        // Both "a b" and "a-b" sanitize to "a-b" — the second collides and gets
        // the "-2" suffix via unique_label.
        assert_eq!(specs[0].label, "a-b");
        assert_eq!(specs[1].label, "a-b-2");
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(t, l)| (t.to_string(), l.to_string()))
            .collect()
    }

    #[test]
    fn configured_specs_pins_open_windows_to_their_live_labels() {
        // The divergence scenario: two titles both sanitize to "a-b". At runtime the
        // window titled "a b" was hot-reload-opened *after* "a-b" already held the
        // base label, so reconcile_ops suffixed it "a-b-2". window_specs (config order,
        // "a b" first) would instead give "a b" the base "a-b" — the disagreement that
        // corrupted the menu. configured_specs must keep the live labels.
        let c = cfg(r##"
[[window]]
title = "a b"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
[[window]]
title = "a-b"
colour = "#222222"
  [[window.tab]]
  title = "t2"
  dir = "/tmp/t2"
"##);
        // Live state: both windows open, on the labels reconcile_ops actually assigned.
        let live_names = map(&[("a b", "a-b-2"), ("a-b", "a-b")]);
        let live_labels = set(&["a-b-2", "a-b"]);
        let specs = configured_specs(&c, &live_names, &live_labels);
        // Pinned to the LIVE labels (not window_specs' config-order assignment).
        assert_eq!(specs[0].title, "a b");
        assert_eq!(specs[0].label, "a-b-2");
        assert_eq!(specs[1].title, "a-b");
        assert_eq!(specs[1].label, "a-b");
    }

    #[test]
    fn configured_specs_assigns_closed_windows_avoiding_live_labels() {
        // Same config, but the "a-b" window is closed (absent from live state) while
        // "a b" stays open on its live "a-b-2". The closed window must get a fresh
        // label that avoids the live one — and it resolves to the same label it held
        // when open, so ⌘⇧T / reopen (which match against last_closed) line up.
        let c = cfg(r##"
[[window]]
title = "a b"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
[[window]]
title = "a-b"
colour = "#222222"
  [[window.tab]]
  title = "t2"
  dir = "/tmp/t2"
"##);
        let live_names = map(&[("a b", "a-b-2")]);
        let live_labels = set(&["a-b-2"]);
        let specs = configured_specs(&c, &live_names, &live_labels);
        assert_eq!(specs[0].label, "a-b-2"); // open → pinned live
        assert_eq!(specs[1].label, "a-b"); // closed → fresh, avoids live "a-b-2"
    }

    #[test]
    fn configured_specs_reserves_diagnostic_label_for_closed_windows() {
        let c = cfg(r##"
[[window]]
title = "warden diagnostic"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
"##);
        // No live windows: the sole window is "closed" and must not grab DIAG_LABEL.
        let specs = configured_specs(&c, &HashMap::new(), &HashSet::new());
        assert_ne!(specs[0].label, DIAG_LABEL);
        assert_eq!(specs[0].label, "warden-diagnostic-2");
    }

    #[test]
    fn window_specs_reserves_diagnostic_label() {
        // A window whose title sanitizes to the reserved diagnostic label must NOT
        // be assigned that label (it would silently break window-state persistence
        // and crash config recovery) — it gets the "-2" suffix instead.
        let c = cfg(r##"
[[window]]
title = "warden diagnostic"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
"##);
        let specs = window_specs(&c);
        assert_ne!(specs[0].label, DIAG_LABEL);
        assert_eq!(specs[0].label, "warden-diagnostic-2");
    }

    /// Regression: a tab added by a hot-reload reconcile (`WindowUpdate.add_tabs`) must
    /// get the same tree/tree_path derivation as the initial `materialize` path
    /// (`window_to_spec`). Before the fix, `reconcile_ops` had no roots in scope for
    /// this construction site and hardcoded `tree: false, tree_path: vec![]`, so a
    /// project discovered by a rescan/hot-reload rendered as a loose tab instead of
    /// nested under its tree root.
    #[test]
    fn reconcile_add_tab_gets_tree_meta_from_new_config_roots() {
        use warden_config::{Colour, Density, Root, Tab, TabDigitKeys, WindowUpdate};

        let added = Tab {
            key: "/r/Dev/gh/lockyc/warden".into(),
            title: "warden".into(),
            dir: "/r/Dev/gh/lockyc/warden".into(),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: Some("Dev".into()),
            probe: None,
            kill: None,
        };
        let recon = Reconciliation {
            open: Vec::new(),
            close: Vec::new(),
            update: vec![WindowUpdate {
                title: "dev".into(),
                colour: None,
                add_tabs: vec![added],
                remove_tabs: Vec::new(),
                tab_order: vec!["/r/Dev/gh/lockyc/warden".into()],
                set_meta: Vec::new(),
            }],
        };
        let new_config = Config {
            windows: vec![Window {
                title: "dev".into(),
                colour: Colour { r: 0, g: 0, b: 0 },
                width: 1500,
                height: 1000,
                tabs: Vec::new(),
                roots: vec![Root {
                    name: "Dev".into(),
                    dir: "/r/Dev".into(),
                    depth: 6,
                    shell: "sh".into(),
                    startup: None,
                    probe: None,
                    kill: None,
                }],
            }],
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            notify_debug: false,
        };
        let name_to_label: HashMap<String, String> = [("dev".to_string(), "dev".to_string())]
            .into_iter()
            .collect();
        let taken: HashSet<String> = HashSet::new();

        let ops = reconcile_ops(&recon, &new_config, &name_to_label, &taken);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            WindowOp::Update { add_tabs, .. } => {
                assert_eq!(add_tabs.len(), 1);
                assert!(add_tabs[0].spec.tree, "added tree tab must be flagged tree");
                assert_eq!(
                    add_tabs[0].spec.tree_path,
                    vec!["gh".to_string(), "lockyc".to_string()]
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }
}
