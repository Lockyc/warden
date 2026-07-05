# Session-presence probing — the scheduler & presence cache

How warden drives the per-tab **cyan session-presence dot**: the off-main-thread
scheduler, the per-window fast-until-stable bursts, the `PresenceCache` that paints dots
instantly on window (re)open, and the invariants that keep it correct and cheap. Read this
before touching `probe.rs`, the presence dot, or anything that bumps/reprobes. The
user-facing `probe`/`kill` config and cascade are in [`docs/config.md`](config.md); the
kill-dot-flicker and `emit_to`-leaks footguns stay in CLAUDE.md's *Conventions & footguns*.

## Probing runs off the main thread and must stamp the window label

The scheduler (`probe::run_scheduler`, spawned once in `main.rs` setup — see *the scheduler
is the single probe driver* below) owns a per-window schedule; each due window is probed
via `probe_window`, which snapshots that window's probe work-list under the `ManagerState`
lock, then **releases the lock before** running `sh -c` (probes are slow; never hold the
mutex across them).

Results emit via `warden:session-state` stamped with the window `label` and filtered by the
chrome's `forMe()` — the same `emit_to`-leaks footgun as `warden:refresh` (see CLAUDE.md) —
**per tab, the moment each tab's probe returns, and only when it changed vs the previous
pass** (`presence_changed` + `emit_one` in `probe.rs`), NOT one batch after the whole
window's pass. This is load-bearing: a window's pass is **sequential** and O(tabs), so a
`[[window.root]]` over a big tree (e.g. `~/Developer` → dozens of discovered project tabs)
makes a single pass *seconds* long. A batched end-of-pass emit instead traps the
killed/started tab's new state behind every *other* tab's probe (the visible "dot takes
ages to clear after kill" lag). The `changed`-only guard also means a **settled window's
pass emits nothing**, while the full result map is still recorded into `PresenceCache`
every pass. **Don't** revert to a single end-of-pass emit, and **don't** add a targeted
one-shot reprobe of the killed tab to "clear it faster" — that violates
scheduler-is-the-only-prober; per-tab emit is the in-scheduler fix.

`probe_interval` is shared as an `Arc<AtomicU64>` (the scheduler's slow floor) so a
hot-reload changes cadence live — the reload hook pairs the new floor with a `bump_all`, so
every window re-bursts and re-settles onto it (a re-read of the atomic alone does not re-arm
an already-Idle/Slow window's far-future next-due; the paired bump does). Each window also
gets an initial fast burst from the chrome's `probe_now` command (→ `probe::bump`), invoked
**only after** its `warden:session-state` listener is registered (`await sessionListenReady`
in `index.html`'s `init()`) — so the first emit can't be lost to the listener-registration
race. **Don't** reintroduce a launch-time direct probe call to "populate early" — it races
the listener and the dot can stay hollow; bump instead.

### The `PresenceCache` paints dots on the first render

First *paint* of the dots does not wait for that burst: the scheduler records every pass
into the manager's persistent **`PresenceCache`** (keyed window-label → tab-id → present?,
*outliving* window close/reopen — a reopen rebuilds the Registry from scratch), and
`init_dto`/refresh patch each `TabDto.presence` from it, so a (re)opened window renders its
cyan dots from the last-known state on the **first** render (`toComponentDto` folds
`t.presence` in ahead of the "off" default) instead of hollow-until-first-probe. The burst
keeps the dots *live* (live `warden:session-state` events win over the cached seed); the
cache is what makes open/reopen paint instantly, because a bump's emit is dropped by a
webview whose listener isn't alive yet. A genuine first-ever open has an empty cache (dots
hollow until the first probe records) — strictly no worse than pre-cache. **Don't** tie the
cache to the Registry (empty exactly when a reopen needs it) or clear it on window close
(that defeats the reopen-paint).

### Probe execution details

Probe `exit 0 = session present`; cwd = the tab's dir; tokens `{dir}`/`{title}` are
substituted **raw** (not shell-quoted), so quote them in the command (`'… "{dir}"'`) when a
path/title may contain spaces or `sh` metacharacters — otherwise the probe word-splits and
silently reports "no session"; stdout/stderr are discarded so a chatty probe can't spam
warden. Due windows are probed **sequentially** on the scheduler thread (never the UI
thread), each bounded by a per-probe timeout (`probe.rs::PROBE_TIMEOUT`, a few seconds): a
wedged probe (e.g. a hung tmux) is killed and reported absent rather than freezing every
window's dot — but a slow-but-under-timeout probe still stalls that tick's pass, so keep
probe commands fast. A **spawn/exec failure** (broken command — wrong path, missing binary)
is distinguished from a clean non-zero exit and logged via `eprintln!` (still "no dot", just
diagnosable) so a misconfigured probe isn't a silent permanently-hollow dot. Keep warden
tmux/amux-agnostic — the command is the user's, warden only reads its exit code.

## The scheduler is the single probe driver — never reintroduce a one-shot reprobe

Every trigger (`activate_tab`, window focus, `probe_now`, `kill_session`, `start_session`,
hot-reload, `rescan_root`) calls `probe::bump`/`bump_all` to push a window into a fast
burst; none of them call `probe_window`/`run_probe` directly or run their own reprobe loop.
Burst state (`WindowSchedule` in `probe.rs`) is tracked **per window, deliberately** — one
window's flapping probe shouldn't force every other window's dots into fast polling too.
`CAP` bounds a burst that never settles (a flapping/nondeterministic probe): **don't**
remove it, or a bad probe command pins the scheduler at `FAST` forever. There is **no
optimistic dot-clear** by design (see CLAUDE.md's kill-flicker footgun) — re-adding a
chrome-side clear, even a "helpful" one, reintroduces the flicker once a stale pass can land
mid-teardown. `probe_interval = 0` means **event-driven-then-idle** — burst on every
trigger, then no steady polling until the next one — not "no probing"; don't read a `0`
floor as disabling presence checks.

`run_scheduler` must be spawned **exactly once** per process (`main.rs` setup does this,
right after `ManagerState` is managed). Spawning a second one won't crash, but it's far
worse than a harmless duplicate: each `run_scheduler` makes its own `mpsc` channel and moves
the sender into `install_bump_tx`, whose `OnceLock` keeps only the **first** — the second
call's sender is dropped on the spot, leaving that thread's receiver **disconnected**. A
disconnected `recv_timeout(TICK)` returns *immediately* every iteration instead of blocking
for `TICK`, so the loop busy-spins with no throttle — pegging a CPU core and hammering the
`ManagerState` lock — on top of duplicating probes on any due window. So a stray second
spawn shows up as a wedged core, not a quiet extra poll. **Don't** spawn one from a
hot-reload path or "to help it catch up."
