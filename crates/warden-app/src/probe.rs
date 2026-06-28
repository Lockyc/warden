//! Session-presence probes. warden runs a user-configured shell command per
//! tab and reports exit-0 (= "a session exists") to the chrome. Generic by
//! design — warden knows nothing about tmux/amux; the command lives in config.

use crate::ManagerState;
use std::path::Path;
use std::process::Command;
use tauri::{AppHandle, Emitter, Manager};

/// Substitute the per-tab tokens into a probe command. `{dir}` → working
/// directory, `{title}` → tab title. Other text is left verbatim.
pub fn substitute(probe: &str, dir: &Path, title: &str) -> String {
    probe
        .replace("{dir}", &dir.to_string_lossy())
        .replace("{title}", title)
}

/// Run `cmd` via `sh -c` with cwd = `dir`. `true` iff it exits 0 (session
/// present). A non-zero exit OR a spawn failure → `false`.
pub fn run_probe(cmd: &str, dir: &Path) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Synchronously probe one window (`Some(label)`) or all (`None`) and emit a
/// label-stamped `warden:session-state` per window. Snapshots the work-list
/// under the manager lock, then releases it BEFORE running the (slow) probes.
pub fn run_pass(app: &AppHandle, only: Option<&str>) {
    let Some(state) = app.try_state::<ManagerState>() else {
        return;
    };
    // (label, Vec<(id, dir, title, probe)>) snapshot — lock held only here.
    let per_window = {
        let m = state.lock();
        m.probe_targets(only)
    };
    for (label, tabs) in per_window {
        if tabs.is_empty() {
            continue;
        }
        let mut states = serde_json::Map::new();
        for (id, dir, title, probe) in tabs {
            let cmd = substitute(&probe, &dir, &title);
            states.insert(id, serde_json::Value::Bool(run_probe(&cmd, &dir)));
        }
        let _ = app.emit_to(
            label.as_str(),
            "warden:session-state",
            serde_json::json!({ "label": label, "states": states }),
        );
    }
}

/// Run `run_pass` on a detached thread (for the focus/refresh one-shots, which
/// must not block the main thread). `sh -c` is fine off-thread; AppHandle is Send.
pub fn spawn_pass(app: AppHandle, only: Option<String>) {
    std::thread::spawn(move || run_pass(&app, only.as_deref()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn substitute_replaces_dir_and_title() {
        let out = substitute("x {title} {dir} y", &PathBuf::from("/tmp/p"), "proj");
        assert_eq!(out, "x proj /tmp/p y");
    }

    #[test]
    fn substitute_leaves_unknown_text_verbatim() {
        let out = substitute("check-session --name proj", &PathBuf::from("/tmp"), "proj");
        assert_eq!(out, "check-session --name proj");
    }

    #[test]
    fn run_probe_true_for_exit_zero() {
        assert!(run_probe("true", &PathBuf::from("/tmp")));
        assert!(run_probe("exit 0", &PathBuf::from("/tmp")));
    }

    #[test]
    fn run_probe_false_for_nonzero_exit() {
        assert!(!run_probe("false", &PathBuf::from("/tmp")));
        assert!(!run_probe("exit 3", &PathBuf::from("/tmp")));
    }

    #[test]
    fn run_probe_runs_in_dir() {
        // `test "$(basename "$PWD")" = tmp` exits 0 only if cwd is /tmp.
        assert!(run_probe(
            "test \"$(basename \"$PWD\")\" = tmp",
            &PathBuf::from("/tmp")
        ));
    }
}
