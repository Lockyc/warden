//! Filesystem discovery of git projects under a `[[window.root]]` dir. Pure over a
//! directory tree (no AppKit/Tauri) so it unit-tests against temp dirs. Stops at every
//! `.git` (dir or file — worktrees use a file), never descends into a git root, skips
//! hidden dirs, and does not follow symlinks. Results feed the effective-config scanner
//! that synthesizes project tabs (see plan.rs / manager.rs).

use std::path::{Path, PathBuf};
use warden_config::{Root, Tab};

/// True if `dir` is a git root (`.git` dir or file present).
fn is_git_root(dir: &Path) -> bool {
    let dot = dir.join(".git");
    dot.exists()
}

/// Recursive worker: push git roots found at or below `dir`. `remaining` is the depth
/// budget below `dir` (0 = may match `dir` itself but not descend).
fn walk(dir: &Path, remaining: u32, out: &mut Vec<PathBuf>) {
    if is_git_root(dir) {
        out.push(dir.to_path_buf());
        return; // never descend into a git root — no sub-repos
    }
    if remaining == 0 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // unreadable dir → skip silently
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Directories only; skip symlinks (cycle/noise) and hidden/dot dirs.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        walk(&path, remaining - 1, out);
    }
}

/// Absolute git-root project paths beneath `dir`, deterministic (sorted) order.
pub fn scan_root(dir: &Path, max_depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // The root dir itself doesn't count as a project even if it's a repo; start at its
    // children so `max_depth` counts levels *below* `dir`.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            walk(&path, max_depth.saturating_sub(1), &mut out);
        }
    }
    out.sort();
    out
}

/// Folder segments strictly between `root_dir` and `project` (project name excluded).
pub fn tree_path(root_dir: &Path, project: &Path) -> Vec<String> {
    let rel = match project.strip_prefix(root_dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut segs: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    segs.pop(); // drop the project's own dir name
    segs
}

/// Discover the root's projects and turn each into a synthetic `Tab`. Identity is the
/// absolute project path (unique even across same-named projects); the display title is
/// the basename. `group` = the root's name so the existing group-sectioning places these
/// rows under a labelled section. Cascade values (shell/cmd/probe/kill) come from the root.
pub fn synthesize_tabs(root: &Root) -> Vec<Tab> {
    scan_root(&root.dir, root.depth)
        .into_iter()
        .map(|path| {
            let title = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            Tab {
                key: path.to_string_lossy().into_owned(),
                title,
                dir: path,
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
    fn finds_git_roots_and_stops_at_them() {
        let base = tmp("stop");
        git(&base.join("gh/lockyc/warden"));
        // a nested repo inside a git root must NOT be discovered separately
        git(&base.join("gh/lockyc/warden/vendor/sub"));
        git(&base.join("gh/other/proj"));
        fs::create_dir_all(&base.join("gh/empty")).unwrap(); // no repo → nothing
        let mut got = scan_root(&base, 6);
        got.sort();
        assert_eq!(
            got,
            vec![base.join("gh/lockyc/warden"), base.join("gh/other/proj")]
        );
    }

    #[test]
    fn respects_depth_and_skips_hidden() {
        let base = tmp("depth");
        git(&base.join("a/b/c/deep")); // depth 4 below base
        git(&base.join(".hidden/repo")); // hidden dir skipped
        assert!(scan_root(&base, 2).is_empty()); // too shallow to reach it
        assert_eq!(scan_root(&base, 6), vec![base.join("a/b/c/deep")]);
    }

    #[test]
    fn git_file_worktree_counts_as_root() {
        let base = tmp("wt");
        let wt = base.join("worktree");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /somewhere\n").unwrap();
        assert_eq!(scan_root(&base, 6), vec![wt]);
    }

    #[test]
    fn tree_path_is_segments_between_root_and_project() {
        let root = PathBuf::from("/r/Developer");
        assert_eq!(
            tree_path(&root, &PathBuf::from("/r/Developer/gh/lockyc/warden")),
            vec!["gh".to_string(), "lockyc".to_string()]
        );
        assert!(tree_path(&root, &PathBuf::from("/r/Developer/loose")).is_empty());
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
