//! Session-presence probes. warden runs a user-configured shell command per
//! tab and reports exit-0 (= "a session exists") to the chrome. Generic by
//! design — warden knows nothing about tmux/amux; the command lives in config.

use crate::ManagerState;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// Per-probe deadline. Probes run sequentially on the scheduler thread, so one wedged command (a hung
/// `tmux`, a probe that blocks on I/O) would otherwise freeze every window's presence dot until it
/// returns. Bounded here: long enough for a healthy `amux --probe` (sub-second), short enough that
/// a stuck probe can't stall the scheduler pass for more than a few seconds. On timeout the child is killed
/// and the tab treated as absent.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Fast-burst probe interval — the cadence a window polls at while it is "hot" (a key moment
/// just fired) and its session state hasn't settled yet.
pub(crate) const FAST: Duration = Duration::from_millis(400);
/// Scheduler wake granularity — it wakes on a bump OR at least this often to service due windows.
pub(crate) const TICK: Duration = Duration::from_millis(200);
/// Ceiling on a single burst: a flapping/nondeterministic probe never "settles", so without this it
/// would pin the scheduler Fast forever. After CAP the window drops to the slow floor regardless.
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

/// Per-window schedule state owned by the scheduler thread (never shared/locked).
struct WindowSchedule {
    next_due: Instant,
    prev: BTreeMap<String, bool>,
    agree: u32,
    burst_start: Instant,
}

/// A `next_due` far enough in the future to mean "Idle — only a bump wakes this window".
fn idle_due(now: Instant) -> Instant {
    now + Duration::from_secs(365 * 24 * 3600)
}

/// Background presence-probe scheduler. Owns per-window `WindowSchedule`s; wakes on a `bump` or every
/// [`TICK`]. On a bump a window goes Fast (probe asap, reset agreement + burst clock); each due window
/// is probed, its result diffed against the previous pass, and `advance` decides the next cadence
/// (Fast until the state settles for [`AGREE_TARGET`] passes or the burst hits [`CAP`], then the slow
/// floor `interval`, or Idle when `interval == 0`). This is the ONLY probe driver — no other code
/// spawns probe passes; triggers `bump` instead. Run inside a dedicated thread.
pub fn run_scheduler(app: AppHandle, interval: Arc<AtomicU64>) {
    let (tx, rx) = mpsc::channel::<String>();
    install_bump_tx(tx);
    let mut sched: HashMap<String, WindowSchedule> = HashMap::new();

    loop {
        // Block until a bump arrives or TICK elapses, then drain any other queued bumps.
        let first = rx.recv_timeout(TICK);
        let now = Instant::now();
        let mut bumps: Vec<String> = Vec::new();
        if let Ok(l) = first {
            bumps.push(l);
        }
        while let Ok(l) = rx.try_recv() {
            bumps.push(l);
        }
        let slow = interval.load(Ordering::Relaxed);

        // Apply bumps: window goes hot (probe immediately, fresh burst).
        for label in bumps {
            let s = sched.entry(label).or_insert_with(|| WindowSchedule {
                next_due: now,
                prev: BTreeMap::new(),
                agree: 0,
                burst_start: now,
            });
            s.next_due = now;
            s.agree = 0;
            s.burst_start = now;
        }

        // Reconcile the schedule map against currently-live windows: drop closed ones, and add
        // newly-opened ones in a slow/idle state (their first Fast burst comes from the chrome's
        // probe_now bump once its listener is ready — avoids the emit-before-listener race).
        let live: Vec<String> = app
            .try_state::<ManagerState>()
            .map(|st| {
                let m = st.lock();
                m.probe_targets(None).into_iter().map(|(l, _)| l).collect()
            })
            .unwrap_or_default();
        sched.retain(|l, _| live.contains(l));
        for l in &live {
            sched.entry(l.clone()).or_insert_with(|| WindowSchedule {
                next_due: if slow > 0 {
                    now + Duration::from_secs(slow)
                } else {
                    idle_due(now)
                },
                prev: BTreeMap::new(),
                agree: 0,
                burst_start: now,
            });
        }

        // Probe every due window and advance its cadence.
        for (label, s) in sched.iter_mut() {
            if now < s.next_due {
                continue;
            }
            let new = probe_window(&app, label);
            let changed = new != s.prev;
            let (agree, cadence) =
                advance(changed, s.agree, now.duration_since(s.burst_start), slow);
            s.prev = new;
            s.agree = agree;
            s.next_due = match cadence {
                Cadence::Fast => now + FAST,
                Cadence::Slow(d) => now + d,
                Cadence::Idle => idle_due(now),
            };
        }
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

/// Probe one window's tabs and emit its `warden:session-state`, returning the per-tab result map.
/// Snapshots the work-list under the manager lock, then releases it BEFORE running the (slow)
/// probes — never hold the mutex across `sh -c`. Returns an empty map for an unknown/closed label
/// or a window with no probe-enabled tabs (the caller settles that to Idle/Slow trivially).
pub(crate) fn probe_window(app: &AppHandle, label: &str) -> BTreeMap<String, bool> {
    let Some(state) = app.try_state::<ManagerState>() else {
        return BTreeMap::new();
    };
    // (label, Vec<(id, dir, title, probe)>) — lock held only for the snapshot.
    let per_window = {
        let m = state.lock();
        m.probe_targets(Some(label))
    };
    let mut result = BTreeMap::new();
    for (_lbl, tabs) in per_window {
        for (id, dir, title, probe) in tabs {
            let cmd = substitute(&probe, &dir, &title);
            result.insert(id, run_probe(&cmd, &dir));
        }
    }
    if result.is_empty() {
        return result; // nothing to emit; caller compares empty==empty → settles
    }
    let mut states = serde_json::Map::new();
    for (id, on) in &result {
        states.insert(id.clone(), serde_json::Value::Bool(*on));
    }
    let _ = app.emit_to(
        label,
        "warden:session-state",
        serde_json::json!({ "label": label, "states": states }),
    );
    result
}

/// Scheduler bump channel sender, set once by `run_scheduler` at startup. Commands/events call
/// `bump`/`bump_all` to push a window into a fast burst. A `OnceLock` (never re-set) so the sender
/// lives for the whole process — the receiver end never disconnects.
static BUMP_TX: OnceLock<Sender<String>> = OnceLock::new();

/// Install the bump sender (idempotent-ish: only the first call wins). Called by `run_scheduler`.
pub fn install_bump_tx(tx: Sender<String>) {
    let _ = BUMP_TX.set(tx);
}

/// Enqueue a fast-burst request for one window by label. No-op if the scheduler isn't installed
/// (unit tests, teardown) or the receiver is gone.
pub fn bump(label: &str) {
    if let Some(tx) = BUMP_TX.get() {
        let _ = tx.send(label.to_string());
    }
}

/// Bump every currently-live window — used by hot-reload/rescan, which can change multiple windows.
pub fn bump_all(app: &AppHandle) {
    let Some(state) = app.try_state::<ManagerState>() else {
        return;
    };
    let labels: Vec<String> = {
        let m = state.lock();
        m.probe_targets(None).into_iter().map(|(l, _)| l).collect()
    };
    for l in labels {
        bump(&l);
    }
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

    #[test]
    fn bump_before_install_is_a_noop() {
        // bump() must never panic when no scheduler is installed (e.g. a command fires during teardown).
        bump("nonexistent-window"); // no scheduler in a unit-test process → silent no-op
    }
}
