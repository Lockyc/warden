//! Pure bridge: `warden_config` types → app-side window/tab descriptors, plus
//! Tauri window-label sanitization. No AppKit, no Tauri — fully unit-tested.

use crate::surface::TabSpec;
use std::collections::HashSet;
use warden_config::{Config, Profile};

/// A tab to materialize, plus its spawn policy. `keep_alive` drives lazy-vs-eager
/// spawn in the registry (spec §3); the surface layer itself never sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TabPlan {
    pub spec: TabSpec,
    pub keep_alive: bool,
}

/// Everything needed to build one profile window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub label: String,  // sanitized, unique — the Tauri window label
    pub name: String,   // profile name, verbatim — banner + window title
    pub colour: String, // "#rrggbb" from Colour::hex()
    pub tabs: Vec<TabPlan>,
}

/// Map an arbitrary profile name to the Tauri label charset `[A-Za-z0-9-/:_]`.
/// Disallowed chars → '-'; leading/trailing '-' trimmed; empty → "profile".
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
        s = "profile".to_string();
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

/// Build a `WindowSpec` for one profile under an already-chosen `label`.
/// Tab id = `Tab::key` (the resolved title — the reconcile identity).
pub fn profile_to_spec(p: &Profile, label: String) -> WindowSpec {
    let tabs = p
        .tabs
        .iter()
        .map(|t| TabPlan {
            spec: TabSpec {
                id: t.key.clone(),
                title: t.title.clone(),
                dir: t.dir.clone(),
                cmd: t.cmd.clone(),
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

/// Map a whole config to window specs, assigning unique labels in profile order.
pub fn window_specs(config: &Config) -> Vec<WindowSpec> {
    let mut taken = HashSet::new();
    config
        .profiles
        .iter()
        .map(|p| {
            let label = unique_label(&p.name, &taken);
            taken.insert(label.clone());
            profile_to_spec(p, label)
        })
        .collect()
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
        assert_eq!(sanitize_label("☕☕"), "profile");
    }

    #[test]
    fn unique_label_suffixes_on_collision() {
        let mut taken = HashSet::new();
        taken.insert("work".to_string());
        taken.insert("work-2".to_string());
        assert_eq!(unique_label("work", &taken), "work-3");
    }

    #[test]
    fn window_specs_maps_profile_and_tabs() {
        let c = cfg(
            r##"
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  title = "locus"
  dir = "/tmp/locus"
  keep_alive = true
  [[profile.tab]]
  title = "ops"
  dir = "/tmp/ops"
"##,
        );
        let specs = window_specs(&c);
        assert_eq!(specs.len(), 1);
        let w = &specs[0];
        assert_eq!(w.label, "work");
        assert_eq!(w.name, "work");
        assert_eq!(w.colour, "#0f8a8a");
        assert_eq!(w.tabs.len(), 2);
        assert_eq!(w.tabs[0].spec.id, "locus");
        assert_eq!(w.tabs[0].spec.title, "locus");
        assert_eq!(w.tabs[0].keep_alive, true);
        assert_eq!(w.tabs[1].spec.id, "ops");
        assert_eq!(w.tabs[1].keep_alive, false);
    }

    #[test]
    fn window_specs_dedupes_labels_for_colliding_sanitized_names() {
        let c = cfg(
            r##"
[[profile]]
name = "a b"
colour = "#111111"
  [[profile.tab]]
  title = "t1"
  dir = "/tmp/t1"
[[profile]]
name = "a-b"
colour = "#222222"
  [[profile.tab]]
  title = "t2"
  dir = "/tmp/t2"
"##,
        );
        let specs = window_specs(&c);
        // Both "a b" and "a-b" sanitize to "a-b" — the second collides and gets
        // the "-2" suffix via unique_label.
        assert_eq!(specs[0].label, "a-b");
        assert_eq!(specs[1].label, "a-b-2");
    }
}
