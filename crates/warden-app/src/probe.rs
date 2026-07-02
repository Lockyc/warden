//! Session-presence probes. warden runs a user-configured shell command per
//! tab and reports exit-0 (= "a session exists") to the chrome. Generic by
//! design — warden knows nothing about tmux/amux; the command lives in config.

use crate::ManagerState;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// Per-probe deadline. Probes run sequentially on the poll thread, so one wedged command (a hung
/// `tmux`, a probe that blocks on I/O) would otherwise freeze every window's presence dot until it
/// returns. Bounded here: long enough for a healthy `amux --probe` (sub-second), short enough that
/// a stuck probe can't stall the poll for more than a few seconds. On timeout the child is killed
/// and the tab treated as absent.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Substitute the per-tab tokens into a probe command. `{dir}` → working
/// directory, `{title}` → tab title. Other text is left verbatim.
pub fn substitute(probe: &str, dir: &Path, title: &str) -> String {
    probe
        .replace("{dir}", &dir.to_string_lossy())
        .replace("{title}", title)
}

/// Run `cmd` via `sh -c` with cwd = `dir`. `true` iff it exits 0 (session
/// present). All non-present outcomes collapse to `false` — a clean non-zero exit
/// (no session), a spawn/exec failure (broken probe command), or a timeout (wedged
/// probe, killed) — but the *spawn failure* is logged (via `eprintln!`) so a
/// misconfigured probe (wrong path, missing binary) is diagnosable rather than a
/// permanently-hollow dot with no signal. stdout/stderr are otherwise discarded —
/// this runs every `probe_interval` seconds in the background, so a chatty probe (or
/// one whose stderr isn't redirected) must not spam warden. Bounded by
/// [`PROBE_TIMEOUT`] so one stuck probe can't freeze the whole poll.
pub fn run_probe(cmd: &str, dir: &Path) -> bool {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            // Distinct from a clean non-zero exit: the command itself couldn't run (bad path,
            // missing binary, interior NUL). Both render "no dot", but only this one is a
            // misconfiguration — surface it so it's diagnosable in logs.
            eprintln!("warden: probe failed to spawn ({cmd:?} in {dir:?}): {e}");
            return false;
        }
    };

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Wedged probe — kill it and treat the session as absent so it can't stall
                    // the sequential poll (and every other window's dot) indefinitely.
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return false,
        }
    }
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

    #[test]
    fn run_probe_false_for_spawn_failure() {
        // A cwd that can't exist means `sh` itself can't be spawned there → exec failure, not a
        // clean non-zero exit. Treated as absent (and logged), never a hang.
        assert!(!run_probe("true", &PathBuf::from("/no/such/dir/xyzzy")));
    }

    #[test]
    fn run_probe_times_out_wedged_command() {
        // A probe that would block far past the deadline is killed and reported absent, bounded by
        // PROBE_TIMEOUT rather than the sleep duration.
        let start = Instant::now();
        assert!(!run_probe("sleep 60", &PathBuf::from("/tmp")));
        assert!(
            start.elapsed() < PROBE_TIMEOUT + Duration::from_secs(2),
            "probe should return around the timeout, not wait out the sleep"
        );
    }
}
