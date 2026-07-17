//! Session-presence probes. warden runs a user-configured shell command per
//! tab and reports its [`Presence`] (live / recoverable / absent, by exit code)
//! to the chrome. Generic by design — warden knows nothing about tmux/amux;
//! the command lives in config.

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

/// A tab's session state, as reported by its `probe` command's exit code.
///
/// Three states, not two: `Recoverable` is the seam between "there's a session" and "there's
/// nothing" — no live session, but the probe reports a *restorable* one (the canonical amux case:
/// a crashed session a plain `amux` launch would offer to restore). warden renders it as a ghost
/// and keeps the same start affordance, so clicking it is what triggers the restore.
///
/// The exit vocabulary is deliberately sparse so a hand-rolled probe degrades gracefully: a
/// `tmux has-session` one-liner never exits 3, so it simply never ghosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum Presence {
    /// Exit 0 — a live session. Cyan dot; kill affordance when `kill` is configured.
    #[serde(rename = "on")]
    Present,
    /// Exit 3 — no live session, but a restorable one. Ghost; start affordance only.
    #[serde(rename = "ghost")]
    Recoverable,
    /// Every other outcome — clean non-zero, spawn failure, or timeout. Hollow ring.
    #[serde(rename = "off")]
    Absent,
}

impl Presence {
    /// chrome-core's wire value for this state (its presence API speaks these strings).
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Present => "on",
            Presence::Recoverable => "ghost",
            Presence::Absent => "off",
        }
    }
}

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
    prev: BTreeMap<String, Presence>,
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
fn presence_changed(prev: &BTreeMap<String, Presence>, id: &str, on: Presence) -> bool {
    prev.get(id).copied() != Some(on)
}

/// Substitute the per-tab tokens into a probe command. `{dir}` → working
/// directory, `{title}` → tab title. Other text is left verbatim.
pub fn substitute(probe: &str, dir: &Path, title: &str) -> String {
    probe
        .replace("{dir}", &dir.to_string_lossy())
        .replace("{title}", title)
}

/// Run `cmd` via `sh -c` with cwd = `dir`, mapping its exit code to a [`Presence`]:
/// `0` ⇒ [`Presence::Present`] (a live session), `3` ⇒ [`Presence::Recoverable`] (no live session,
/// but a restorable one — warden ghosts the dot), everything else ⇒ [`Presence::Absent`].
///
/// **The collapse direction is load-bearing.** Only a clean exit 3 means recoverable; a spawn/exec
/// failure (broken probe command) and a timeout (wedged probe, killed) both collapse to `Absent`,
/// never `Recoverable` — a misconfigured probe must not ghost every tab in the sidebar. The *spawn
/// failure* is still logged (via `eprintln!`) so a bad path/missing binary is diagnosable rather
/// than a permanently-hollow dot with no signal.
///
/// stdout/stderr are otherwise discarded — this runs every `probe_interval` seconds in the
/// background, so a chatty probe (or one whose stderr isn't redirected) must not spam warden.
/// Bounded by [`PROBE_TIMEOUT`] so one stuck probe can't freeze the whole poll.
pub fn run_probe(cmd: &str, dir: &Path) -> Presence {
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
            return Presence::Absent;
        }
    };

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return match status.code() {
                    Some(0) => Presence::Present,
                    // Only a CLEAN exit 3 ghosts. A signal-killed probe has no code() and lands
                    // in the catch-all below, as it must.
                    Some(3) => Presence::Recoverable,
                    _ => Presence::Absent,
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Wedged probe — kill it and treat the session as absent so it frees its pool
                    // slot instead of tying it up (and starving the sweep) indefinitely.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Presence::Absent;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return Presence::Absent,
        }
    }
}

/// Emit a `warden:session-state` carrying `states` (tab id → `"on"`/`"ghost"`/`"off"`) for one
/// window. The chrome iterates whatever's in `states`, so a partial map (even one entry) updates
/// exactly those dots. Stamped with `label` and filtered by the chrome's `forMe()` (the
/// `emit_to`-leaks footgun, same as `warden:refresh`). No-op on an empty map (nothing to say).
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
fn emit_one(app: &AppHandle, label: &str, id: &str, on: Presence) {
    let mut states = serde_json::Map::new();
    states.insert(
        id.to_string(),
        serde_json::Value::String(on.as_str().to_string()),
    );
    emit_states(app, label, states);
}

/// Replay a window's last-known presence (`id → state`) to its chrome as one batched
/// `warden:session-state`. Called by `probe_now` the moment the chrome's listener is ready, so
/// state a pre-listener probe pass emitted-and-lost (with `prev` now populated, no live pass
/// re-emits it) is still delivered — otherwise those dots stay stuck at their hollow build-time
/// state. See `PresenceCache::snapshot`.
pub fn emit_presence_snapshot(app: &AppHandle, label: &str, states: &BTreeMap<String, Presence>) {
    let map = states
        .iter()
        .map(|(id, on)| {
            (
                id.clone(),
                serde_json::Value::String(on.as_str().to_string()),
            )
        })
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
/// the moment each probe returns and collecting the full `id → Presence` map. Workers pull from a
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
) -> BTreeMap<String, Presence>
where
    P: Fn(&str, &Path) -> Presence + Sync,
    Emit: Fn(&str, Presence) + Sync,
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
                // Hand this result to the caller's observer the instant its own probe finishes (it
                // records it, then emits only if it changed), then keep it for the settle-diff.
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
    prev: &BTreeMap<String, Presence>,
    priority: Option<&str>,
) -> BTreeMap<String, Presence> {
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
    sweep(
        work,
        priority,
        MAX_PROBE_CONCURRENCY,
        run_probe,
        |id, on| {
            observe(
                prev,
                id,
                on,
                |id, on: Presence| state.lock().presence_cache.record_one(label, id, on),
                |id, on| emit_one(app, label, id, on),
            )
        },
    )
}

/// One tab's post-probe bookkeeping: **record first, emit second** — and emit only when the state
/// actually changed, so a settled window's pass stays silent.
///
/// The order is load-bearing, not incidental. A freshly-built webview isn't listening for
/// `warden:session-state` yet, so an emit issued before the chrome's listener registers is simply
/// dropped; the only thing that heals it is `probe_now`'s handshake replay of the PresenceCache
/// (which the chrome triggers the moment its listener IS ready). Recording each result *before* its
/// emit means any dropped emit is already in the cache the replay reads — whereas persisting the
/// whole pass at the END of the sweep leaves a window in which the early tabs' emits are dropped
/// AND the cache the replay reads is still empty. Those dots then stay dark for the life of the
/// process: `prev` already holds the value, so the changed-only guard means no later pass ever
/// re-emits it. That was the "a tab whose amux session is live shows a dark presence dot" bug — a
/// wide `[[window.root]]` makes the first sweep long enough to straddle the handshake, and only
/// tabs that probed *present* look wrong (a dropped `false` leaves a hollow dot, which is what an
/// absent session should look like anyway).
fn observe(
    prev: &BTreeMap<String, Presence>,
    id: &str,
    on: Presence,
    record: impl Fn(&str, Presence),
    emit: impl Fn(&str, Presence),
) {
    record(id, on);
    if presence_changed(prev, id, on) {
        emit(id, on);
    }
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
        assert_eq!(run_probe("true", &PathBuf::from("/tmp")), Presence::Present);
        assert_eq!(
            run_probe("exit 0", &PathBuf::from("/tmp")),
            Presence::Present
        );
    }

    #[test]
    fn run_probe_false_for_nonzero_exit() {
        // exit 3 is the dedicated recoverable code (see `presence_maps_exit_codes_to_three_states`)
        // — this test covers the plain "nothing at all" case, any other nonzero exit.
        assert_eq!(run_probe("false", &PathBuf::from("/tmp")), Presence::Absent);
        assert_eq!(
            run_probe("exit 1", &PathBuf::from("/tmp")),
            Presence::Absent
        );
    }

    #[test]
    fn run_probe_runs_in_dir() {
        // `test "$(basename "$PWD")" = tmp` exits 0 only if cwd is /tmp.
        assert_eq!(
            run_probe(
                "test \"$(basename \"$PWD\")\" = tmp",
                &PathBuf::from("/tmp")
            ),
            Presence::Present
        );
    }

    #[test]
    fn run_probe_false_for_spawn_failure() {
        // A cwd that can't exist means `sh` itself can't be spawned there → exec failure, not a
        // clean non-zero exit. Treated as absent (and logged), never a hang.
        assert_eq!(
            run_probe("true", &PathBuf::from("/no/such/dir/xyzzy")),
            Presence::Absent
        );
    }

    #[test]
    fn run_probe_times_out_wedged_command() {
        // A probe that would block far past the deadline is killed and reported absent, bounded by
        // PROBE_TIMEOUT rather than the sleep duration.
        let start = Instant::now();
        assert_eq!(
            run_probe("sleep 60", &PathBuf::from("/tmp")),
            Presence::Absent
        );
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
    fn presence_maps_exit_codes_to_three_states() {
        let dir = std::path::Path::new("/tmp");
        assert_eq!(
            run_probe("exit 0", dir),
            Presence::Present,
            "exit 0 ⇒ live session"
        );
        assert_eq!(
            run_probe("exit 3", dir),
            Presence::Recoverable,
            "exit 3 ⇒ recoverable"
        );
        assert_eq!(
            run_probe("exit 1", dir),
            Presence::Absent,
            "exit 1 ⇒ nothing"
        );
        assert_eq!(
            run_probe("exit 2", dir),
            Presence::Absent,
            "unknown non-zero ⇒ absent"
        );
        assert_eq!(
            run_probe("exit 7", dir),
            Presence::Absent,
            "unknown non-zero ⇒ absent"
        );
    }

    #[test]
    fn a_broken_probe_is_absent_not_recoverable() {
        // Load-bearing safety direction: a misconfigured probe must never ghost every tab.
        let dir = std::path::Path::new("/tmp");
        assert_eq!(
            run_probe("/nonexistent/binary/xyzzy", dir),
            Presence::Absent,
            "a command that cannot run is absent, never recoverable"
        );
    }

    #[test]
    fn presence_wire_values_match_chrome_core() {
        assert_eq!(Presence::Present.as_str(), "on");
        assert_eq!(Presence::Recoverable.as_str(), "ghost");
        assert_eq!(Presence::Absent.as_str(), "off");
    }

    #[test]
    fn presence_serialization_matches_as_str() {
        for p in [Presence::Present, Presence::Recoverable, Presence::Absent] {
            assert_eq!(
                serde_json::to_value(p).unwrap(),
                serde_json::Value::String(p.as_str().to_string()),
                "serde and as_str must agree — both are the chrome-core wire value"
            );
        }
    }

    #[test]
    fn presence_changed_detects_every_transition_including_ghost() {
        let mut prev = BTreeMap::new();
        prev.insert("a".to_string(), Presence::Present);
        prev.insert("b".to_string(), Presence::Absent);
        prev.insert("g".to_string(), Presence::Recoverable);

        // Unchanged ⇒ silent (a settled window's pass emits nothing).
        assert!(!presence_changed(&prev, "a", Presence::Present));
        assert!(!presence_changed(&prev, "g", Presence::Recoverable));

        // The ghost transitions must emit, or the dot latches.
        assert!(
            presence_changed(&prev, "b", Presence::Recoverable),
            "absent→recoverable (a crash just became restorable) must emit"
        );
        assert!(
            presence_changed(&prev, "g", Presence::Absent),
            "recoverable→absent (the offer burned) must emit"
        );
        assert!(
            presence_changed(&prev, "g", Presence::Present),
            "recoverable→present (the restore landed) must emit"
        );
        assert!(
            presence_changed(&prev, "a", Presence::Recoverable),
            "present→recoverable (the session crashed) must emit"
        );

        // First sight ⇒ changed, so the first pass populates every dot.
        assert!(presence_changed(&prev, "c", Presence::Absent));
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
            |cmd: &str, _dir: &Path| {
                if cmd.contains('y') {
                    Presence::Present
                } else {
                    Presence::Absent
                }
            },
            |id, on| emitted.lock().unwrap().push((id.to_string(), on)),
        );
        assert_eq!(map.get("y1"), Some(&Presence::Present));
        assert_eq!(map.get("n1"), Some(&Presence::Absent));
        assert_eq!(map.get("y2"), Some(&Presence::Present));
        let mut e = emitted.lock().unwrap().clone();
        e.sort();
        assert_eq!(
            e,
            vec![
                ("n1".to_string(), Presence::Absent),
                ("y1".to_string(), Presence::Present),
                ("y2".to_string(), Presence::Present),
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
                Presence::Present
            },
            |id, _on| emitted.lock().unwrap().push(id.to_string()),
        );
        assert_eq!(map.len(), 20, "every tab probed");
        assert!(map.values().all(|&v| v == Presence::Present));
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
    fn observe_records_before_it_emits() {
        // The order is the fix for the stuck-dark dot: a probe result must be in the cache before
        // the emit that a not-yet-registered listener may drop, because probe_now's replay reads
        // that cache at exactly the moment the listener comes up.
        let log = Mutex::new(Vec::new());
        observe(
            &BTreeMap::new(),
            "t0",
            Presence::Present,
            |id, on| log.lock().unwrap().push(format!("record {id}={on:?}")),
            |id, on| log.lock().unwrap().push(format!("emit {id}={on:?}")),
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "record t0=Present".to_string(),
                "emit t0=Present".to_string()
            ],
            "record must precede emit"
        );
    }

    #[test]
    fn observe_records_an_unchanged_result_but_stays_silent() {
        // A settled pass emits nothing — but must still keep the cache current, since the cache is
        // what a (re)opened window and the handshake replay paint from.
        let mut prev = BTreeMap::new();
        prev.insert("t0".to_string(), Presence::Present);
        let recorded = Mutex::new(Vec::new());
        let emitted = Mutex::new(Vec::new());
        observe(
            &prev,
            "t0",
            Presence::Present,
            |id, on| recorded.lock().unwrap().push((id.to_string(), on)),
            |id, on| emitted.lock().unwrap().push((id.to_string(), on)),
        );
        assert_eq!(
            *recorded.lock().unwrap(),
            vec![("t0".to_string(), Presence::Present)]
        );
        assert!(
            emitted.lock().unwrap().is_empty(),
            "unchanged → no re-emit (a settled window's pass is silent)"
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
                Presence::Present
            },
            |_id, _on| {},
        );
        assert_eq!(first.lock().unwrap().as_deref(), Some("d"));
    }
}
