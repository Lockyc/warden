//! Filesystem discovery of git projects under a `[[window.root]]` dir. Scanning itself
//! (`scan_root`/`tree_path`/`discover_projects` — pure over a directory tree, no AppKit/Tauri,
//! stops at every `.git`, never descends into a git root, skips hidden dirs, does not follow
//! symlinks) lives in config-core, shared with lector — leaf-free, so it hands back a bare
//! `DiscoveredProject` rather than knowing about warden's `Tab`. This module's only job is that
//! mapping: a root's discovered projects → warden's own synthetic `Tab`s. Results feed the
//! effective-config scanner that synthesizes project tabs (see plan.rs / manager.rs).

use warden_config::{discover_projects, Root, RootDir, Tab};

/// Discover the root's projects (via config-core's `discover_projects`) and turn each into a
/// synthetic `Tab`. Identity is the absolute project path (unique even across same-named
/// projects); the display title is the basename. `group` = the root's name so the existing
/// group-sectioning places these rows under a labelled section. Cascade values
/// (shell/cmd/probe/kill) come from the root.
pub fn synthesize_tabs(root: &Root) -> Vec<Tab> {
    let root_dir = RootDir {
        name: root.name.clone(),
        dir: root.dir.clone(),
        depth: root.depth,
    };
    discover_projects(std::slice::from_ref(&root_dir))
        .into_iter()
        .map(|proj| {
            let title = proj
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| proj.path.to_string_lossy().into_owned());
            Tab {
                id: None,
                key: proj.path.to_string_lossy().into_owned(),
                title,
                dir: proj.path,
                shell: root.shell.clone(),
                startup: root.startup.clone(),
                load_on_open: false,
                group: Some(root.name.clone()),
                probe: root.probe.clone(),
                kill: root.kill.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn tmp(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("warden-scan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }
    fn git(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
    }

    #[test]
    fn overlapping_roots_dedup_to_one_tab_per_path() {
        use warden_config::{Colour, Config, Density, Root, TabDigitKeys, Window};
        let base = tmp("overlap");
        git(&base.join("proj"));
        let mk_root = |name: &str| Root {
            name: name.into(),
            dir: base.clone(),
            depth: 6,
            shell: "sh".into(),
            startup: None,
            probe: None,
            kill: None,
        };
        // Two roots pointing at the SAME dir → the same project discovered twice.
        let cfg = Config {
            windows: vec![Window {
                title: "w".into(),
                colour: Colour { r: 0, g: 0, b: 0 },
                width: 1500,
                height: 1000,
                open_on_start: true,
                tabs: Vec::new(),
                roots: vec![mk_root("A"), mk_root("B")],
            }],
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
        };
        let eff = crate::manager::effective_config(&cfg);
        let tabs = &eff.windows[0].tabs;
        // The project appears exactly once, under the FIRST root's section.
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].key, base.join("proj").to_string_lossy());
        assert_eq!(tabs[0].group.as_deref(), Some("A"));
    }

    #[test]
    fn curated_tab_shadows_same_dir_discovered_project() {
        use warden_config::{Colour, Config, Density, Root, Tab, TabDigitKeys, Window};
        let base = tmp("shadow");
        let proj = base.join("proj");
        git(&proj);
        let root = Root {
            name: "Developer".into(),
            dir: base.clone(),
            depth: 6,
            shell: "sh".into(),
            startup: None,
            probe: None,
            kill: None,
        };
        // A curated tab with no explicit `id`, whose `dir` is the SAME project the root
        // discovers: its key collapses to the normalized dir (`resolve.rs::normalize_dir_key`),
        // which equals the discovered tab's path key exactly — the collision `effective_config`
        // resolves by keeping the curated tab (first in `window.tabs`, before roots' synthesized
        // tabs are appended).
        let curated = Tab {
            id: None,
            key: proj.to_string_lossy().into_owned(),
            title: "curated-title".into(),
            dir: proj.clone(),
            shell: "fish -l".into(),
            startup: None,
            load_on_open: false,
            group: None,
            probe: None,
            kill: None,
        };
        let cfg = Config {
            windows: vec![Window {
                title: "w".into(),
                colour: Colour { r: 0, g: 0, b: 0 },
                width: 1500,
                height: 1000,
                open_on_start: true,
                tabs: vec![curated],
                roots: vec![root],
            }],
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
        };
        let eff = crate::manager::effective_config(&cfg);
        let tabs = &eff.windows[0].tabs;
        assert_eq!(
            tabs.len(),
            1,
            "curated tab + same-dir discovered project must collapse to one row"
        );
        assert_eq!(
            tabs[0].title, "curated-title",
            "the curated tab shadows the discovered project, not the other way round"
        );
        assert_eq!(
            tabs[0].group, None,
            "shadowing keeps the curated tab's own (loose) grouping, not the root's section"
        );
    }

    #[test]
    fn synthesizes_project_tabs_from_a_root() {
        use warden_config::Root;
        let base = tmp("syn");
        git(&base.join("gh/lockyc/warden"));
        git(&base.join("solo"));
        let root = Root {
            name: "Developer".into(),
            dir: base.clone(),
            depth: 6,
            shell: "gsh -l".into(),
            startup: Some("run".into()),
            probe: Some("p".into()),
            kill: None,
        };
        let mut tabs = synthesize_tabs(&root);
        tabs.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(tabs.len(), 2);
        let warden = tabs.iter().find(|t| t.title == "warden").unwrap();
        assert_eq!(warden.key, base.join("gh/lockyc/warden").to_string_lossy());
        assert_eq!(warden.group.as_deref(), Some("Developer"));
        assert_eq!(warden.shell, "gsh -l");
        assert_eq!(warden.startup.as_deref(), Some("run"));
        assert_eq!(warden.probe.as_deref(), Some("p"));
        assert!(!warden.load_on_open);
    }
}
