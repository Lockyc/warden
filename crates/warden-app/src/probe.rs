//! Session-presence probes. warden runs a user-configured shell command per
//! tab and reports exit-0 (= "a session exists") to the chrome. Generic by
//! design — warden knows nothing about tmux/amux; the command lives in config.

use crate::ManagerState;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// One tab's probe work-item: `(id, dir, title, probe_cmd)` — the snapshot shape `probe_targets`
/// yields and the unit a sweep worker runs.
type ProbeWork = (String, PathBuf, String, String);

/// Upper bound on how many tab probes run at once in a single window's sweep. Probes are
/// process-spawn / socket-check bound (not CPU bound), so this can exceed the core count; it caps
/// the burst of concurrent `sh -c` children so a wide `[[window.root]]` (dozens of discovered tabs)
/// can't fork a hundred processes at once. With this cap a ~40-tab window sweeps in ~3 waves of one
/// probe (~0.07s each) instead of ~40 sequential probes (~3s) — the fix for the "dot takes ages to
/// clear after kill" lag. The just-bumped tab is probed first (see `order_work`) so it lands in the
/// first wave regardless of its list position.
const MAX_PROBE_CONCURRENCY: usize = 16;

/// Per-probe deadline. A window's probes run concurrently on a bounded pool, but a wedged command (a
/// hung `tmux`, a probe that blocks on I/O) would still tie up one pool slot — and enough wedged
/// probes could starve the pool and stall the sweep. Bounded here: long enough for a healthy
/// `amux --probe` (sub-second), short enough that a stuck probe frees its slot within a few seconds.
/// On timeout the child is killed and the tab treated as absent.
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
    /// The tab a tab-specific bump (kill/start/activate) named, to probe first on the next pass.
    /// Set by such a bump, `take`n when the window is probed (applies to that one pass only). A
    /// whole-window bump (hot-reload/rescan/focus) leaves it unset — natural list order.
    priority: Option<String>,
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
    let (tx, rx) = mpsc::channel::<Bump>();
    install_bump_tx(tx);
    let mut sched: HashMap<String, WindowSchedule> = HashMap::new();

    loop {
        // Block until a bump arrives or TICK elapses, then drain any other queued bumps.
        let first = rx.recv_timeout(TICK);
        let now = Instant::now();
        let mut bumps: Vec<Bump> = Vec::new();
        if let Ok(b) = first {
            bumps.push(b);
        }
        while let Ok(b) = rx.try_recv() {
            bumps.push(b);
        }
        let slow = interval.load(Ordering::Relaxed);

        // Apply bumps: window goes hot (probe immediately, fresh burst).
        for Bump { label, tab } in bumps {
            let s = sched.entry(label).or_insert_with(|| WindowSchedule {
                next_due: now,
                prev: BTreeMap::new(),
                agree: 0,
                burst_start: now,
                priority: None,
            });
            s.next_due = now;
            s.agree = 0;
            s.burst_start = now;
            // A tab-specific bump names the tab to probe first; a whole-window bump (tab == None)
            // must not clear a priority a coalesced tab-bump set in the same drain.
            if tab.is_some() {
                s.priority = tab;
            }
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
                priority: None,
            });
        }

        // Probe every due window and advance its cadence.
        for (label, s) in sched.iter_mut() {
            if now < s.next_due {
                continue;
            }
            // Priority applies to this one pass; take it so the next pass reverts to list order.
            let priority = s.priority.take();
            let new = probe_window(&app, label, &s.prev, priority.as_deref());
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

/// Whether tab `id`'s freshly-probed `on` differs from the previous pass's value — i.e. whether
/// this pass must re-emit its dot. A tab absent from `prev` (never probed, or a fresh window)
/// counts as changed, so the first pass populates every dot. Emitting only changed tabs (paired
/// with the concurrent, priority-first sweep) lets a killed/started tab's dot update within ~one
/// probe, and keeps a settled window's pass silent instead of re-emitting all N states every tick.
fn presence_changed(prev: &BTreeMap<String, bool>, id: &str, on: bool) -> bool {
    prev.get(id).copied() != Some(on)
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
                    // Wedged probe — kill it and treat the session as absent so it frees its pool
                    // slot instead of tying it up (and starving the sweep) indefinitely.
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

/// Emit a `warden:session-state` carrying `states` (tab id → present) for one window. The chrome
/// iterates whatever's in `states`, so a partial map (even one entry) updates exactly those dots.
/// Stamped with `label` and filtered by the chrome's `forMe()` (the `emit_to`-leaks footgun, same
/// as `warden:refresh`). No-op on an empty map (nothing to say).
fn emit_states(app: &AppHandle, label: &str, states: serde_json::Map<String, serde_json::Value>) {
    if states.is_empty() {
        return;
    }
    let _ = app.emit_to(
        label,
        "warden:session-state",
        serde_json::json!({ "label": label, "states": states }),
    );
}

/// Emit one tab's presence as a single-entry `warden:session-state` (see `emit_states`).
fn emit_one(app: &AppHandle, label: &str, id: &str, on: bool) {
    let mut states = serde_json::Map::new();
    states.insert(id.to_string(), serde_json::Value::Bool(on));
    emit_states(app, label, states);
}

/// Replay a window's last-known presence (`id → present`) to its chrome as one batched
/// `warden:session-state`. Called by `probe_now` the moment the chrome's listener is ready, so
/// state a pre-listener probe pass emitted-and-lost (with `prev` now populated, no live pass
/// re-emits it) is still delivered — otherwise those dots stay stuck at their hollow build-time
/// state. See `PresenceCache::snapshot`.
pub fn emit_presence_snapshot(app: &AppHandle, label: &str, states: &BTreeMap<String, bool>) {
    let map = states
        .iter()
        .map(|(id, on)| (id.clone(), serde_json::Value::Bool(*on)))
        .collect();
    emit_states(app, label, map);
}

/// Reorder `work` so the bumped tab `priority` is probed **first**. A trigger that acts on one tab
/// (kill/start/activate) carries that tab's id through the bump; probing it first means its dot lands
/// in the sweep's first concurrency wave regardless of where it sits in the list — so a killed tab
/// deep in a wide `[[window.root]]` clears in ~one probe, not after its whole batch. No-op when
/// `priority` is `None` or names a tab not in the work-list. Relative order of the other tabs is
/// preserved (a rotate of the `[0..=i]` prefix), so a whole-window bump (no priority) leaves the
/// natural list order untouched.
fn order_work(work: &mut [ProbeWork], priority: Option<&str>) {
    if let Some(pid) = priority {
        if let Some(i) = work.iter().position(|(id, ..)| id == pid) {
            work[..=i].rotate_right(1);
        }
    }
}

/// Run every work-item's probe **concurrently** (bounded by `concurrency`), calling `emit(id, on)`
/// the moment each probe returns and collecting the full `id → present?` map. Workers pull from a
/// shared cursor, so the sweep's wall-clock is ~`ceil(n/concurrency)` probe times, not the sum —
/// the fix for the O(tabs) sequential pass that made a wide window's dots take seconds to update.
/// `order_work` runs first, so the just-bumped tab is at the head of the cursor and is claimed in
/// the first wave. Pure w.r.t. Tauri (takes `probe_fn`/`emit` closures) so it's unit-testable.
fn sweep<P, Emit>(
    mut work: Vec<ProbeWork>,
    priority: Option<&str>,
    concurrency: usize,
    probe_fn: P,
    emit: Emit,
) -> BTreeMap<String, bool>
where
    P: Fn(&str, &Path) -> bool + Sync,
    Emit: Fn(&str, bool) + Sync,
{
    order_work(&mut work, priority);
    if work.is_empty() {
        return BTreeMap::new();
    }
    let n = concurrency.max(1).min(work.len());
    let cursor = Mutex::new(work.into_iter());
    let result = Mutex::new(BTreeMap::new());
    thread::scope(|scope| {
        for _ in 0..n {
            scope.spawn(|| loop {
                // Claim the next item and release the cursor lock BEFORE probing — never hold it
                // across the (slow) probe, or the pool serializes on it.
                let next = cursor.lock().unwrap().next();
                let Some((id, dir, title, probe)) = next else {
                    break;
                };
                let cmd = substitute(&probe, &dir, &title);
                let on = probe_fn(&cmd, &dir);
                // Emit this dot the instant its own probe finishes (the caller's closure filters to
                // changed-only), then record it for the settle-diff + PresenceCache.
                emit(&id, on);
                result.lock().unwrap().insert(id, on);
            });
        }
    });
    result.into_inner().unwrap()
}

/// Probe one window's tabs **concurrently** (bounded by [`MAX_PROBE_CONCURRENCY`]) and emit
/// `warden:session-state` **per tab, the moment each probe returns** (and only when it changed vs
/// `prev` — the previous pass's result), returning the full per-tab result map for the scheduler's
/// settle-diff. Snapshots the work-list under the manager lock, then releases it BEFORE running the
/// (slow) probes — never hold the mutex across `sh -c`. Returns an empty map for an unknown/closed
/// label or a window with no probe-enabled tabs (the caller settles that to Idle/Slow trivially).
///
/// `priority` is the just-bumped tab (kill/start/activate), probed first so its dot lands in the
/// first wave. Concurrency + per-tab emit together make a killed/started tab's dot update within
/// ~one probe (~0.1s) even in a `[[window.root]]` over a large tree — the pass no longer costs
/// O(tabs) in wall-clock, which was the "the dot takes ages to clear after I kill the session" lag.
/// The `changed`-only guard keeps a settled window's pass silent instead of re-emitting all N states.
pub(crate) fn probe_window(
    app: &AppHandle,
    label: &str,
    prev: &BTreeMap<String, bool>,
    priority: Option<&str>,
) -> BTreeMap<String, bool> {
    let Some(state) = app.try_state::<ManagerState>() else {
        return BTreeMap::new();
    };
    // (label, Vec<(id, dir, title, probe)>) — lock held only for the snapshot.
    let work: Vec<ProbeWork> = {
        let m = state.lock();
        m.probe_targets(Some(label))
            .into_iter()
            .flat_map(|(_lbl, tabs)| tabs)
            .collect()
    };
    let result = sweep(
        work,
        priority,
        MAX_PROBE_CONCURRENCY,
        run_probe,
        |id, on| {
            // Emit only on a real change, so a stable window's pass stays silent.
            if presence_changed(prev, id, on) {
                emit_one(app, label, id, on);
            }
        },
    );
    if result.is_empty() {
        return result; // nothing emitted; caller compares empty==empty → settles
    }
    // Persist this pass so a (re)opened window's init/refresh DTO paints its dots from the
    // last-known state — the chrome's `warden:session-state` listener isn't alive yet when a
    // freshly-built webview opens, so the per-tab emits above would be dropped for it. See
    // PresenceCache.
    {
        let mut m = state.lock();
        m.presence_cache.record(label, &result);
    }
    result
}

/// A fast-burst request for one window. `tab` names the tab that triggered it (kill/start/activate)
/// so the scheduler probes that tab first; `None` is a whole-window bump (hot-reload/rescan/focus).
struct Bump {
    label: String,
    tab: Option<String>,
}

/// Scheduler bump channel sender, set once by `run_scheduler` at startup. Commands/events call
/// `bump`/`bump_tab`/`bump_all` to push a window into a fast burst. A `OnceLock` (never re-set) so
/// the sender lives for the whole process — the receiver end never disconnects.
static BUMP_TX: OnceLock<Sender<Bump>> = OnceLock::new();

/// Install the bump sender (idempotent-ish: only the first call wins). Called by `run_scheduler`.
fn install_bump_tx(tx: Sender<Bump>) {
    let _ = BUMP_TX.set(tx);
}

/// Enqueue a fast-burst request for one window by label. No-op if the scheduler isn't installed
/// (unit tests, teardown) or the receiver is gone. Use `bump_tab` when a single tab triggered it.
pub fn bump(label: &str) {
    send_bump(label, None);
}

/// Like `bump`, but names the tab that triggered the burst so the scheduler probes it first — its
/// dot then updates within the first concurrency wave regardless of its list position. Used by the
/// tab-specific triggers (kill/start/activate).
pub fn bump_tab(label: &str, tab: &str) {
    send_bump(label, Some(tab.to_string()));
}

fn send_bump(label: &str, tab: Option<String>) {
    if let Some(tx) = BUMP_TX.get() {
        let _ = tx.send(Bump {
            label: label.to_string(),
            tab,
        });
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
    fn presence_changed_detects_transitions_and_first_sight() {
        let mut prev = BTreeMap::new();
        prev.insert("a".to_string(), true);
        prev.insert("b".to_string(), false);
        // Unchanged → no re-emit (a settled window's pass stays silent).
        assert!(!presence_changed(&prev, "a", true));
        assert!(!presence_changed(&prev, "b", false));
        // Transitions → re-emit. The kill case is present→absent; start is absent→present.
        assert!(
            presence_changed(&prev, "a", false),
            "kill (present→absent) must emit"
        );
        assert!(
            presence_changed(&prev, "b", true),
            "start (absent→present) must emit"
        );
        // Never-probed tab (fresh window / newly-added tab) → emit so the first pass populates it.
        assert!(presence_changed(&prev, "c", false));
        assert!(presence_changed(&prev, "c", true));
    }

    #[test]
    fn bump_before_install_is_a_noop() {
        // bump()/bump_tab() must never panic when no scheduler is installed (e.g. a command fires
        // during teardown).
        bump("nonexistent-window"); // no scheduler in a unit-test process → silent no-op
        bump_tab("nonexistent-window", "some-tab");
    }

    // Each tab's probe command is set to its own id, so a `probe_fn` can key on it.
    fn work(ids: &[&str]) -> Vec<ProbeWork> {
        ids.iter()
            .map(|s| {
                (
                    s.to_string(),
                    PathBuf::from("/tmp"),
                    s.to_string(),
                    s.to_string(),
                )
            })
            .collect()
    }

    fn ids_of(w: &[ProbeWork]) -> Vec<String> {
        w.iter().map(|(id, ..)| id.clone()).collect()
    }

    #[test]
    fn order_work_moves_priority_to_front_preserving_rest() {
        let mut w = work(&["a", "b", "c", "d"]);
        order_work(&mut w, Some("c"));
        assert_eq!(ids_of(&w), ["c", "a", "b", "d"]); // c to front, rest keep relative order
    }

    #[test]
    fn order_work_noop_for_absent_or_none_priority() {
        let mut w = work(&["a", "b", "c"]);
        order_work(&mut w, Some("zz")); // not in the list → unchanged
        assert_eq!(ids_of(&w), ["a", "b", "c"]);
        order_work(&mut w, None); // whole-window bump → natural order
        assert_eq!(ids_of(&w), ["a", "b", "c"]);
        order_work(&mut w, Some("a")); // already first → unchanged
        assert_eq!(ids_of(&w), ["a", "b", "c"]);
    }

    #[test]
    fn sweep_returns_full_map_and_emits_every_tab_with_its_result() {
        let emitted = Mutex::new(Vec::new());
        // probe cmd == id (see `work`), so present iff the id contains 'y'.
        let map = sweep(
            work(&["y1", "n1", "y2"]),
            None,
            4,
            |cmd, _dir| cmd.contains('y'),
            |id, on| emitted.lock().unwrap().push((id.to_string(), on)),
        );
        assert_eq!(map.get("y1"), Some(&true));
        assert_eq!(map.get("n1"), Some(&false));
        assert_eq!(map.get("y2"), Some(&true));
        let mut e = emitted.lock().unwrap().clone();
        e.sort();
        assert_eq!(
            e,
            vec![
                ("n1".to_string(), false),
                ("y1".to_string(), true),
                ("y2".to_string(), true),
            ]
        );
    }

    #[test]
    fn sweep_bounds_concurrency_and_probes_all() {
        use std::sync::atomic::AtomicUsize;
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let emitted = Mutex::new(Vec::<String>::new());
        let ids: Vec<String> = (0..20).map(|i| format!("t{i}")).collect();
        let items = work(&ids.iter().map(String::as_str).collect::<Vec<_>>());
        let map = sweep(
            items,
            None,
            4, // cap
            |_cmd, _dir| {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(15)); // force overlap
                live.fetch_sub(1, Ordering::SeqCst);
                true
            },
            |id, _on| emitted.lock().unwrap().push(id.to_string()),
        );
        assert_eq!(map.len(), 20, "every tab probed");
        assert!(map.values().all(|&v| v));
        assert_eq!(emitted.lock().unwrap().len(), 20, "every tab emitted");
        let peak = peak.load(Ordering::SeqCst);
        assert!(
            peak > 1,
            "probes should actually run concurrently (peak {peak})"
        );
        assert!(
            peak <= 4,
            "concurrency must not exceed the cap (peak {peak})"
        );
    }

    #[test]
    fn sweep_probes_priority_tab_first() {
        // concurrency 1 makes claim-order == probe-order, so recording the first-probed cmd (== id)
        // proves the priority tab led despite sitting 4th in the list.
        use std::sync::atomic::AtomicUsize;
        let seq = AtomicUsize::new(0);
        let first = Mutex::new(None);
        let _ = sweep(
            work(&["a", "b", "c", "d", "e"]),
            Some("d"),
            1,
            |cmd, _dir| {
                if seq.fetch_add(1, Ordering::SeqCst) == 0 {
                    *first.lock().unwrap() = Some(cmd.to_string());
                }
                true
            },
            |_id, _on| {},
        );
        assert_eq!(first.lock().unwrap().as_deref(), Some("d"));
    }
}
