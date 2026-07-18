//! Pure bridge: `warden_config` types → app-side window/tab descriptors, plus
//! Tauri window-label sanitization. No AppKit, no Tauri — fully unit-tested.

use crate::surface::TabSpec;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use warden_config::{Config, Reconciliation, Tab, Window};

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

/// Build a `TabPlan` for one resolved `Tab` given its window's root dirs. The single
/// site that maps a `warden_config::Tab` to the app's `TabPlan`/`TabSpec` — used by
/// `window_to_spec` and by `reconcile_ops` for both added and respawned tabs.
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
            tree,
            tree_path,
        },
        load_on_open: t.load_on_open,
    }
}

/// Everything needed to build one window window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub label: String,       // sanitized, unique — the Tauri window label
    pub title: String,       // window title, verbatim — banner + window title
    pub colour: String,      // "#rrggbb" from Colour::hex()
    pub width: f64,          // inner width in logical pixels
    pub height: f64,         // inner height in logical pixels
    pub open_on_start: bool, // false = start closed-but-configured (home surface/menu opens it)
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

/// The opaque token identifying a popped-out tab's detached window, built from its origin
/// window label + the tab's id (= `Tab::key`). Combined and sanitized to the Tauri label
/// charset so `shell_core::detach::detached_label(token)` is always a valid, distinct label:
/// two different tabs (different id) never collide, and the same tab can't be popped out twice
/// (its registry slot goes `Detached` on the first pop-out, so a second `detach` is a no-op).
/// The value is never parsed back — the manager tracks `origin_label` + `tab_id` on the
/// `DetachedSurface` itself — so the colon separator is purely for human legibility of the label.
pub fn detach_window_token(origin_label: &str, tab_id: &str) -> String {
    sanitize_label(&format!("{origin_label}:{tab_id}"))
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
        open_on_start: p.open_on_start,
        tabs,
    }
}

/// Map a whole config to window specs, assigning unique labels in window order.
pub fn window_specs(config: &Config) -> Vec<WindowSpec> {
    // Reserve the shared home surface's label so a config window whose title
    // sanitizes to it (e.g. "shell home") gets `-2`, not a collision that
    // silently breaks window-state persistence / crashes config recovery.
    let mut taken = HashSet::new();
    taken.insert(shell_core::home::HOME_LABEL.to_string());
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
/// label (`live_labels`) ∪ the home surface's reservation ∪ labels assigned earlier here.
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
    taken.insert(shell_core::home::HOME_LABEL.to_string());
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
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WindowMenuEntry {
    pub label: String,
    pub title: String,
    pub open: bool,
    pub colour: String, // "#rrggbb" — for the shared home surface's swatches; the app menu ignores it
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
            colour: s.colour.clone(),
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
        set_meta: Vec<MetaPlan>,
        // Kept tabs whose terminal spec changed — respawn in place (remove+add by id).
        respawn_tabs: Vec<TabPlan>,
    },
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
    // the home surface's label.
    assigned.insert(shell_core::home::HOME_LABEL.to_string());

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
        let win = new_config.windows.iter().find(|w| w.title == u.title);
        let root_dirs: HashMap<&str, &Path> = win
            .map(|w| {
                w.roots
                    .iter()
                    .map(|r| (r.name.as_str(), r.dir.as_path()))
                    .collect()
            })
            .unwrap_or_default();
        let add_tabs: Vec<TabPlan> = u
            .add_tabs
            .iter()
            .map(|t| tab_to_plan(&root_dirs, t))
            .collect();
        let respawn_tabs: Vec<TabPlan> = u
            .respawn_tabs
            .iter()
            .map(|t| tab_to_plan(&root_dirs, t))
            .collect();
        // Recompute each kept tab's tree/tree_path from the NEW config's roots + its
        // (new) group — the same derivation `tab_to_plan` runs for adds/respawns. A
        // shadow flip (curated tab appears/disappears at a discovered project's dir)
        // is a set_meta group change on a kept key, so its tree-ness must follow the
        // new group, not linger from the old one.
        let set_meta: Vec<MetaPlan> = u
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
        ops.push(WindowOp::Update {
            label: label.clone(),
            colour: u.colour.map(|c| c.hex()),
            add_tabs,
            remove_tabs: u.remove_tabs.clone(),
            order: u.tab_order.clone(),
            set_meta,
            respawn_tabs,
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
            open_on_start: true,
            tabs: Vec::new(),
        }
    }

    #[test]
    fn window_specs_carry_open_on_start() {
        let cfg = warden_config::resolve::resolve(
            warden_config::raw::parse(
                r##"
[[window]]
title = "a"
open_on_start = false
  [[window.tab]]
  dir = "/tmp"

[[window]]
title = "b"
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap()
        .0;
        let specs = window_specs(&cfg);
        assert_eq!(specs.len(), 2);
        assert!(!specs[0].open_on_start, "explicit false carries through");
        assert!(specs[1].open_on_start, "default true carries through");
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
        assert_eq!(entries[0].colour, "#000000"); // from spec()'s colour
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
        use warden_config::{Colour, Root, Tab, Window};
        let root_dir = std::path::PathBuf::from("/r/Dev");
        let win = Window {
            title: "dev".into(),
            colour: Colour { r: 0, g: 0, b: 0 },
            width: 1500,
            height: 1000,
            open_on_start: true,
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
    fn configured_specs_reserves_home_label_for_closed_windows() {
        let c = cfg(r##"
[[window]]
title = "shell home"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
"##);
        // No live windows: the sole window is "closed" and must not grab HOME_LABEL.
        let specs = configured_specs(&c, &HashMap::new(), &HashSet::new());
        assert_ne!(specs[0].label, shell_core::home::HOME_LABEL);
        assert_eq!(specs[0].label, "shell-home-2");
    }

    #[test]
    fn window_specs_reserves_home_label() {
        // A window whose title sanitizes to the reserved home-surface label must NOT
        // be assigned that label (it would silently break window-state persistence
        // and collide with the shared home surface) — it gets the "-2" suffix instead.
        let c = cfg(r##"
[[window]]
title = "shell home"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
"##);
        let specs = window_specs(&c);
        assert_ne!(specs[0].label, shell_core::home::HOME_LABEL);
        assert_eq!(specs[0].label, "shell-home-2");
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
                respawn_tabs: Vec::new(),
            }],
        };
        let new_config = Config {
            windows: vec![Window {
                title: "dev".into(),
                colour: Colour { r: 0, g: 0, b: 0 },
                width: 1500,
                height: 1000,
                open_on_start: true,
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
            auto_update: true,
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

    /// Regression: a `[[window.root]]`-discovered project (tree row, `group` = root name)
    /// that a curated tab then shadows at the same `dir` (moving it into a plain group) is a
    /// KEPT tab — same `Tab::key` — so reconcile emits a `set_meta` group change, not add/
    /// remove. Its `tree`/`tree_path` must be recomputed from the new group and cleared;
    /// leaving them stale rendered the shadowed project as its own tree section under the
    /// curated group's name (a "TOOLS" tree row instead of a plain TOOLS group tab).
    #[test]
    fn reconcile_shadow_transition_clears_stale_tree_meta_via_set_meta() {
        use warden_config::{Colour, Density, Root, Tab, TabDigitKeys};

        let key = "/r/Dev/gh/lockyc/mycelium".to_string();
        let root = Root {
            name: "Developer".into(),
            dir: "/r/Dev".into(),
            depth: 6,
            shell: "sh".into(),
            startup: None,
            probe: None,
            kill: None,
        };
        let mk_window = |tabs: Vec<Tab>| Window {
            title: "w".into(),
            colour: Colour { r: 0, g: 0, b: 0 },
            width: 1500,
            height: 1000,
            open_on_start: true,
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
        };
        let new = mk_cfg(vec![curated]);

        let recon = warden_config::reconcile(&old, &new);
        // Same key + changed group ⇒ a kept-tab metadata update, not add/remove.
        assert_eq!(recon.update.len(), 1, "expected one window update");
        assert!(
            recon.update[0].add_tabs.is_empty() && recon.update[0].remove_tabs.is_empty(),
            "shadow transition must be a kept tab, not add/remove"
        );

        let name_to_label: HashMap<String, String> =
            [("w".to_string(), "w".to_string())].into_iter().collect();
        let ops = reconcile_ops(&recon, &new, &name_to_label, &HashSet::new());
        let WindowOp::Update { set_meta, .. } = ops
            .iter()
            .find(|o| matches!(o, WindowOp::Update { .. }))
            .expect("expected an Update op")
        else {
            unreachable!()
        };
        let m = set_meta
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
        let ops = reconcile_ops(&r, &new, &name_label_map(&old), &taken(&old));
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            WindowOp::Update { respawn_tabs, .. } => {
                assert_eq!(respawn_tabs.len(), 1);
                assert_eq!(respawn_tabs[0].spec.id, "a");
                assert_eq!(
                    respawn_tabs[0].spec.dir,
                    std::path::PathBuf::from("/tmp/a-new")
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }
}
