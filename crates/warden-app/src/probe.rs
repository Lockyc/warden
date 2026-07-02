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

/// Fast-burst probe interval — the cadence a window polls at while it is "hot" (a key moment
/// just fired) and its session state hasn't settled yet.
pub(crate) const FAST: Duration = Duration::from_millis(400);
/// Scheduler wake granularity — it wakes on a bump OR at least this often to service due windows.
pub(crate) const TICK: Duration = Duration::from_millis(200);
/// Ceiling on a single burst: a flapping/nondeterministic probe never "settles", so without this it
/// would pin the poll thread Fast forever. After CAP the window drops to the slow floor regardless.
pub(crate) const CAP: Duration = Duration::from_secs(20);
/// Consecutive unchanged passes that count as "settled" (~AGREE_TARGET * FAST of agreement).
pub(crate) const AGREE_TARGET: u32 = 3;

/// The cadence a window should adopt after a probe pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Cadence {
    /// Keep bursting at [`FAST`] — state hasn't settled and the burst hasn't hit [`CAP`].
    Fast,
    /// Settled (or capped): poll at the slow floor after this delay.
    Slow(Duration),
    /// Settled (or capped) with `probe_interval == 0`: event-driven only, no steady poll.
    Idle,
}

/// Pure fast-until-stable transition. Given whether this pass's states changed vs the previous pass,
/// the running agreement count, how long the current burst has run, and the slow-floor seconds,
/// return the new agreement count and the cadence to adopt next. Settling requires
/// [`AGREE_TARGET`] consecutive unchanged passes; a burst is force-ended at [`CAP`] so a flapping
/// signal can't burst forever. `slow_secs == 0` maps a settled/capped window to [`Cadence::Idle`].
pub(crate) fn advance(
    changed: bool,
    agree: u32,
    elapsed: Duration,
    slow_secs: u64,
) -> (u32, Cadence) {
    let agree = if changed { 0 } else { agree + 1 };
    let settled = agree >= AGREE_TARGET;
    let capped = elapsed >= CAP;
    if settled || capped {
        if slow_secs == 0 {
            (agree, Cadence::Idle)
        } else {
            (agree, Cadence::Slow(Duration::from_secs(slow_secs)))
        }
    } else {
        (agree, Cadence::Fast)
    }
}

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
        // Force any tab with a kill in flight to "absent" — its probe above may have
        // observed the still-alive pre-kill session (probes are slow; a kill can land
        // mid-pass), and emitting `true` would re-light the dot the chrome just dropped
        // optimistically (the off→on→off flicker). Checked here, at emit time, so a kill
        // that arrives *during* this pass is still caught. `kill_session` unmarks the tab
        // before its own reprobe, so the true post-kill state (absent, or present if the
        // kill failed) still reaches the chrome. See WindowManager::killing.
        {
            let m = state.lock();
            for (id, val) in states.iter_mut() {
                if m.is_killing(&label, id) {
                    *val = serde_json::Value::Bool(false);
                }
            }
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

    #[test]
    fn advance_settles_after_agree_target_unchanged_passes() {
        // Two agreeing passes are not enough; the third (agree reaches AGREE_TARGET) settles.
        let (a1, c1) = advance(false, 0, Duration::from_millis(400), 5);
        assert_eq!(a1, 1);
        assert!(matches!(c1, Cadence::Fast));
        let (a2, c2) = advance(false, a1, Duration::from_millis(800), 5);
        assert_eq!(a2, 2);
        assert!(matches!(c2, Cadence::Fast));
        let (a3, c3) = advance(false, a2, Duration::from_millis(1200), 5);
        assert_eq!(a3, 3);
        assert!(matches!(c3, Cadence::Slow(d) if d == Duration::from_secs(5)));
    }

    #[test]
    fn advance_change_resets_agreement_and_stays_fast() {
        let (a, c) = advance(true, 2, Duration::from_millis(1200), 5);
        assert_eq!(a, 0);
        assert!(matches!(c, Cadence::Fast));
    }

    #[test]
    fn advance_settled_with_zero_slow_goes_idle() {
        // probe_interval == 0 → event-driven only: after settling, Idle (no steady poll).
        let (_, c) = advance(false, AGREE_TARGET - 1, Duration::from_millis(1200), 0);
        assert!(matches!(c, Cadence::Idle));
    }

    #[test]
    fn advance_caps_a_flapping_signal_to_slow() {
        // Never agrees (changed every pass) but the burst exceeded CAP → drop out of Fast anyway.
        let (a, c) = advance(true, 0, CAP + Duration::from_secs(1), 5);
        assert_eq!(a, 0);
        assert!(matches!(c, Cadence::Slow(d) if d == Duration::from_secs(5)));
    }

    #[test]
    fn advance_cap_with_zero_slow_goes_idle() {
        let (_, c) = advance(true, 0, CAP + Duration::from_secs(1), 0);
        assert!(matches!(c, Cadence::Idle));
    }
}
