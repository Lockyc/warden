---
type: reference
links:
  - rel: part-of
    to: CLAUDE.md
    note: CLAUDE.md points here for the full probe-scheduler internals
---

# Session-presence probing — the scheduler & presence cache

How warden drives the per-tab **three-state session-presence dot** (cyan / ghost / hollow): the off-main-thread
scheduler, the per-window fast-until-stable bursts, the `PresenceCache` that paints dots
instantly on window (re)open, and the invariants that keep it correct and cheap. Read this
before touching `probe.rs`, the presence dot, or anything that bumps/reprobes. The
user-facing `probe`/`kill` config and cascade are in [`docs/config.md`](config.md); the
kill-dot-flicker and `emit_to`-leaks footguns stay in CLAUDE.md's *Conventions & footguns*.

## Probing runs off the main thread and must stamp the window label

The scheduler (`probe::run_scheduler`, spawned once in `main.rs` setup — see *the scheduler
is the single probe driver* below) owns a per-window schedule; each due window is probed
via `probe_window`, which snapshots that window's probe work-list under the `ManagerState`
lock, then **releases the lock before** running any `sh -c` (probes are slow; never hold the
mutex across them).

The window's tabs are then probed **concurrently** on a bounded pool (`sweep` in `probe.rs`,
capped at `MAX_PROBE_CONCURRENCY`), not one at a time: workers pull from a shared cursor, so
a window's pass costs ~`ceil(tabs / concurrency)` probe times in wall-clock, **not the sum**.
This is load-bearing: a `[[window.root]]` over a big tree (e.g. `~/Developer` → dozens of
discovered project tabs) used to make a single *sequential* pass *seconds* long, so a killed
tab's dot only cleared once its probe ran in list order — the "dot takes ages to clear after
kill" lag. Concurrency collapses that to a couple of waves (~0.1–0.2s). Keep the pool
**bounded** — an unbounded sweep would fork a hundred `sh -c` children at once on a wide root.

Two things ride on top of the concurrent sweep, both load-bearing:

- **The just-bumped tab is probed first.** A tab-specific trigger (kill/start/activate) calls
  `bump_tab`/`bump_tab_await`, which stashes that tab as the window's `priority`; `order_work`
  moves it to the head of the work-list so it's claimed in the **first** wave regardless of its
  list position — its dot updates in ~one probe. This is **not** a one-shot reprobe (still one
  driver, still the normal pass); it's just ordering, so it doesn't violate
  scheduler-is-the-only-prober. A whole-window bump (`bump`/`bump_all`, e.g. hot-reload/rescan/
  focus) sets no priority and keeps natural list order. (The `_await` variant additionally holds
  the burst open until the tab's expected transition lands — see the directional-await section.)
- **Emit is per-tab, changed-only, `label`-stamped.** Each worker emits `warden:session-state`
  (stamped with the window `label`, filtered by the chrome's `forMe()` — the same
  `emit_to`-leaks footgun as `warden:refresh`, see CLAUDE.md) the instant *its own* probe
  returns, and only when it **changed vs the previous pass** (`presence_changed` + `emit_one`).
  So a **settled window's pass emits nothing**, while every result — changed or not — is still
  recorded into `PresenceCache`, **per tab and before its emit** (`observe`; see the replay
  section below, where that order is load-bearing). **Don't** revert to a single end-of-pass emit
  (it would trap the killed tab's new state behind the slowest worker), and **don't** serialize
  the sweep back to one-at-a-time.

  The changed-only guard is over the **3-state** `Presence`, so a `Present → Recoverable` transition
  (the session crashed) and a `Recoverable → Absent` one (the offer burned) each emit exactly once.
  `run_probe`'s collapse is what keeps this honest: only a *clean* exit 3 is `Recoverable` — a spawn
  failure or a timeout is `Absent`, so a broken probe can't flip every dot to a ghost and back.

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

First *paint* of the dots does not wait for that burst: the scheduler records every probe result
into the manager's persistent **`PresenceCache`** (keyed window-label → tab-id → `Presence`
on/ghost/off, *outliving* window close/reopen — a reopen rebuilds the Registry from scratch), and
`init_dto`/refresh patch each `TabDto.presence` from it, so a (re)opened window renders its
cyan dots from the last-known state on the **first** render (`toComponentDto` folds
`t.presence` in ahead of the "off" default) instead of hollow-until-first-probe. The burst
keeps the dots *live* (live `warden:session-state` events win over the cached seed); the
cache is what makes open/reopen paint instantly, because a bump's emit is dropped by a
webview whose listener isn't alive yet. A genuine first-ever open has an empty cache (dots
hollow until the first probe records) — strictly no worse than pre-cache. **Don't** tie the
cache to the Registry (empty exactly when a reopen needs it) or clear it on window close
(that defeats the reopen-paint).

**`probe_now` replays the cache snapshot — the listener-race the first-render seed can't
cover.** `init_dto` seeds `TabDto.presence` from the cache at **build** time, when a
first-ever window's cache is still empty. A probe pass can then finish in the gap **between**
that build and the chrome's `session-state` listener registering: its per-tab emits are
dropped (no listener yet), and — `prev` now populated — **no later pass re-emits** them
(`changed == false` forever). So a tab whose state never changes after that lost pass — most
sharply one whose **session pre-existed launch**, `true` from the very first probe — stays
stuck at its hollow build-time dot. The fix: `probe_now` (called by the chrome the instant its
listener *is* ready) reads `PresenceCache::snapshot(label)` and emits it as one batched
`warden:session-state` **before** bumping — replaying the last-known state to the now-ready
listener. This gap was invisible while probe passes were slow (sequential) — the pass was still
emitting when the listener came up, so most emits landed; the concurrent sweep finishes inside
the gap, making the loss reliable, which is what surfaced it. **Don't** drop the `probe_now`
snapshot expecting the live burst alone to paint first state — the burst it triggers is
`changed`-gated against a `prev` the lost pass already filled.

**The replay is only as good as the cache is *current* — so each result is recorded BEFORE its
emit (`observe`), never batched at the end of the pass.** The snapshot covers a pass that
*finished* before the handshake, but a sweep can also still be **in flight** across it: a wide
`[[window.root]]` window probes dozens of tabs in waves, so its early tabs emit (dropped — no
listener) while the pass is far from done. Persisting the whole result map at the end of the
sweep therefore left a window in which those emits were already lost **and** the cache the
replay reads was still empty — and since `prev` holds the value, no later pass ever re-emits it.
Those dots then stay dark for the life of the process, and only tabs that probed **present**
look wrong (a dropped `false` leaves a hollow dot, which is what an absent session looks like
anyway) — the "my amux session is live but the tab's presence dot is dark" bug. Recording
per-tab, before the emit, closes it: anything a dropped emit carried is already in the cache
the replay reads. **Don't** move the record back to the end of the pass "to take the lock once".

### Probe execution details

Probe `exit 0 = session present`, `exit 3 = recoverable` (see `run_probe`'s exit-code map in
`probe.rs`, the vocabulary's single source of truth); cwd = the tab's dir; tokens `{dir}`/`{title}` are
substituted **raw** (not shell-quoted), so quote them in the command (`'… "{dir}"'`) when a
path/title may contain spaces or `sh` metacharacters — otherwise the probe word-splits and
silently reports "no session"; stdout/stderr are discarded so a chatty probe can't spam
warden. A window's tabs are probed **concurrently** on a bounded worker pool off the UI
thread (never on it), and due windows are serviced one after another within a scheduler tick.
Each probe is bounded by a per-probe timeout (`probe.rs::PROBE_TIMEOUT`, a few seconds): a
wedged probe (e.g. a hung tmux) is killed and reported absent rather than tying up its pool
slot forever — but enough slow-but-under-timeout probes can still starve the pool and stall
that tick's pass, so keep probe commands fast. A **spawn/exec failure** (broken command — wrong path, missing binary)
is distinguished from a clean non-zero exit and logged via `eprintln!` (still "no dot", just
diagnosable) so a misconfigured probe isn't a silent permanently-hollow dot. Keep warden
tmux/amux-agnostic — the command is the user's, warden only reads its exit code.

**Probe cost is CPU, and it is the probe command's — not the scheduler's.** A probe is ~92% CPU
(mostly `sys`: it is `fork`/`exec` work, not a blocking socket wait), so a sweep's peak scales with
pool width, and a burst of the canonical `amux --probe` shows up as a periodic kernel-time spike on
the E-cores — the background QoS puts it there deliberately. `probe_interval` cannot bound any of
this: the burst rate is fixed, so raising the floor only widens the gaps between bursts. The levers
are the probe command's own cost and the tab count. `probe.rs::MAX_PROBE_CONCURRENCY` carries the
measured per-probe decomposition and where the time actually goes; the fix for a slow canonical
probe belongs in amux, which owns the session naming and socket layout.

## The scheduler is the single probe driver — never reintroduce a one-shot reprobe

Every trigger pushes a window into a fast burst via a `bump`, and **none** call
`probe_window`/`run_probe` directly or run their own reprobe loop. Three bump flavours:
`bump`/`bump_all` (whole window — `probe_now`, window focus, hot-reload, `rescan_root`);
`bump_tab(label, id)` (a specific tab, no definite expected transition — a warm tab-switch),
which names the tab to probe first that pass (see the priority-first note above); and
`bump_tab_await(label, id, want_present)` (below), which does the same **and** arms a
directional await. Burst state (`WindowSchedule` in `probe.rs`) is tracked **per window,
deliberately** — one window's flapping probe shouldn't force every other window's dots into
fast polling too. `CAP` bounds a burst that never settles (a flapping/nondeterministic probe,
**or a stuck await**): **don't** remove it, or a bad probe command pins the scheduler at
`FAST` forever. There is **no optimistic dot-clear** by design (see CLAUDE.md's kill-flicker
footgun) — re-adding a chrome-side clear, even a "helpful" one, reintroduces the flicker once
a stale pass can land mid-teardown. `probe_interval = 0` means **event-driven-then-idle** —
burst on every trigger, then no steady polling until the next one — not "no probing"; don't
read a `0` floor as disabling presence checks.

### A directional await keeps the burst hot until the expected transition lands

`bump_tab_await(label, id, want_present)` fixes the case a plain burst mishandles: a trigger
that expects a **specific async transition** — a kill (session should go *down*) or a session
start (a cold-`activate` of a probe-enabled tab, or `start_session` re-typing `cmd` — should
come *up*). The transition is asynchronous: `amux --kill` returns before tmux teardown fully
lands, and a started session takes a second or two to spin up. A plain burst treats each "still
the old state" pass as *agreement*, so ~`AGREE_TARGET`×`FAST` (~1.2s) of the transition-not-yet-
landed **settles** the window onto the pre-transition state and drops it to the slow floor
(`probe_interval`, default 5s) — the real change is then only caught on the next slow poll (the
"~5s dot lag after kill/start" bug).

The await closes that: it stashes `(id, want_present)` on the `WindowSchedule`, and the settle
logic (`advance`, gated by `await_pending`) refuses to settle while that tab is still on the old
side — the burst stays `Fast`, agreement reset, until the tab reaches the expected side (then it
resumes normal fast-until-stable settling) **or** `CAP` fires (the backstop for a transition that
never arrives — a failed kill, a session that never comes up — after which it settles normally so
a stuck await can't burst forever). The await is **directional, not leave-baseline**, which is
what makes it self-satisfying: an `activate` of an already-live tab arms `want_present = true` and
`await_pending` returns false on the first pass (already `Present`), so a plain tab-switch never
bursts to `CAP`. It is a **polling-duration heuristic only** — dots are always driven by the
per-tab changed-only emit, so a mis-judged await can at worst poll `Fast` a bit longer, never
paint a wrong dot. **This replaces `start_session`'s old fixed 1s/3s re-bump band-aid** — a single
`bump_tab_await` now covers a slow start (don't reintroduce the delayed re-bumps). `activate_tab`
arms it only for a **fresh cold-spawn of a probe-enabled tab** (`Registry::activate` reports the
fresh spawn; `tab_has_probe` gates it) so a warm switch or a dotless tab stays a plain `bump_tab`.

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
