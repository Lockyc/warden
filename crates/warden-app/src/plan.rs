//! Pure bridge: `warden_config` types → app-side window/tab descriptors, plus
//! Tauri window-label sanitization. No AppKit, no Tauri — fully unit-tested.

use crate::surface::TabSpec;
use std::collections::{HashMap, HashSet};
use warden_config::{Config, Reconciliation, Window};

/// A tab to materialize, plus its spawn policy. `keep_alive` drives lazy-vs-eager
/// spawn in the registry (spec §3); the surface layer itself never sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TabPlan {
    pub spec: TabSpec,
    pub keep_alive: bool,
}

/// Everything needed to build one window window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub label: String,  // sanitized, unique — the Tauri window label
    pub name: String,   // window name, verbatim — banner + window title
    pub colour: String, // "#rrggbb" from Colour::hex()
    pub tabs: Vec<TabPlan>,
}

/// Map an arbitrary window name to the Tauri label charset `[A-Za-z0-9-/:_]`.
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
/// Tab id = `Tab::key` (the resolved title — the reconcile identity).
pub fn window_to_spec(p: &Window, label: String) -> WindowSpec {
    let tabs = p
        .tabs
        .iter()
        .map(|t| TabPlan {
            spec: TabSpec {
                id: t.key.clone(),
                title: t.title.clone(),
                dir: t.dir.clone(),
                shell: t.shell.clone(),
                startup: t.startup.clone(),
                group: t.group.clone(),
            },
            keep_alive: t.keep_alive,
        })
        .collect();
    WindowSpec {
        label,
        name: p.name.clone(),
        colour: p.colour.hex(),
        tabs,
    }
}

/// Map a whole config to window specs, assigning unique labels in window order.
pub fn window_specs(config: &Config) -> Vec<WindowSpec> {
    let mut taken = HashSet::new();
    config
        .windows
        .iter()
        .map(|p| {
            let label = unique_label(&p.name, &taken);
            taken.insert(label.clone());
            window_to_spec(p, label)
        })
        .collect()
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
        // (tab id, new group) for kept tabs whose [[window.group]] changed —
        // re-sections the sidebar without respawning. None = back to loose.
        set_groups: Vec<(String, Option<String>)>,
    },
}

/// Map a reconciliation (by window name) to window ops (by Tauri label).
/// New windows get fresh unique labels avoiding `taken` ∪ labels already
/// assigned earlier in this same call.
pub fn reconcile_ops(
    recon: &Reconciliation,
    name_to_label: &HashMap<String, String>,
    taken: &HashSet<String>,
) -> Vec<WindowOp> {
    let mut ops = Vec::new();
    let mut assigned: HashSet<String> = taken.clone();

    for window in &recon.open {
        let label = unique_label(&window.name, &assigned);
        assigned.insert(label.clone());
        ops.push(WindowOp::Open(window_to_spec(window, label)));
    }

    for name in &recon.close {
        if let Some(label) = name_to_label.get(name) {
            ops.push(WindowOp::Close(label.clone()));
        }
    }

    for u in &recon.update {
        let Some(label) = name_to_label.get(&u.name) else {
            continue;
        };
        let add_tabs = u
            .add_tabs
            .iter()
            .map(|t| TabPlan {
                spec: TabSpec {
                    id: t.key.clone(),
                    title: t.title.clone(),
                    dir: t.dir.clone(),
                    shell: t.shell.clone(),
                    startup: t.startup.clone(),
                    group: t.group.clone(),
                },
                keep_alive: t.keep_alive,
            })
            .collect();
        ops.push(WindowOp::Update {
            label: label.clone(),
            colour: u.colour.map(|c| c.hex()),
            add_tabs,
            remove_tabs: u.remove_tabs.clone(),
            order: u.tab_order.clone(),
            set_groups: u.set_groups.clone(),
        });
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_config::{load, Config};

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
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
  keep_alive = true
  [[window.tab]]
  title = "ops"
  dir = "/tmp/ops"
"##);
        let specs = window_specs(&c);
        assert_eq!(specs.len(), 1);
        let w = &specs[0];
        assert_eq!(w.label, "work");
        assert_eq!(w.name, "work");
        assert_eq!(w.colour, "#0f8a8a");
        assert_eq!(w.tabs.len(), 2);
        assert_eq!(w.tabs[0].spec.id, "locus");
        assert_eq!(w.tabs[0].spec.title, "locus");
        assert!(w.tabs[0].keep_alive);
        assert_eq!(w.tabs[1].spec.id, "ops");
        assert!(!w.tabs[1].keep_alive);
    }

    use warden_config::reconcile;

    fn name_label_map(c: &Config) -> HashMap<String, String> {
        window_specs(c)
            .into_iter()
            .map(|w| (w.name, w.label))
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
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##);
        let r = reconcile(&old, &new);
        let ops = reconcile_ops(&r, &name_label_map(&old), &taken(&old));
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            WindowOp::Open(spec) => assert_eq!(spec.name, "work"),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn closed_window_becomes_close_op_with_label() {
        let old = cfg(r##"
[[window]]
name = "work zone"
colour = "#0f8a8a"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##);
        let new = cfg("");
        let r = reconcile(&old, &new);
        let ops = reconcile_ops(&r, &name_label_map(&old), &taken(&old));
        assert_eq!(ops, vec![WindowOp::Close("work-zone".to_string())]);
    }

    #[test]
    fn colour_change_becomes_update_op_with_hex() {
        let base = r##"
[[window]]
name = "work"
colour = "{C}"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##;
        let old = cfg(&base.replace("{C}", "#0f8a8a"));
        let new = cfg(&base.replace("{C}", "#112233"));
        let r = reconcile(&old, &new);
        let ops = reconcile_ops(&r, &name_label_map(&old), &taken(&old));
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
    fn window_specs_dedupes_labels_for_colliding_sanitized_names() {
        let c = cfg(r##"
[[window]]
name = "a b"
colour = "#111111"
  [[window.tab]]
  title = "t1"
  dir = "/tmp/t1"
[[window]]
name = "a-b"
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
}
