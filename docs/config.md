---
type: reference
links:
  - rel: part-of
    to: CLAUDE.md
    note: CLAUDE.md points here as the full config schema reference
---

# Config schema & resolution

The full reference for warden's config file — every key, the cascade rules, and the
validation semantics. Read this before working on config parsing/resolution
(`warden-config`) or anything that consumes the resolved model. CLAUDE.md carries only a
summary and points here; the probe **scheduler** internals live in `docs/probing.md`, and
the config-adjacent traps (tab identity, format-on-save, the probe/kill exec environment)
are in CLAUDE.md's *Conventions & footguns*.

Config file: `~/.config/warden/config.toml` (override with `WARDEN_CONFIG`).

```toml
shell = "fish -l"                      # optional; global default shell every tab spawns (default: your login shell, run as login)
cmd   = "amux"                         # optional; global default startup command run inside the shell
format_on_save = true                  # optional; default false — rewrite this file tidy on each clean hot-reload
tab_digit_keys = "jump"                # optional; whole-app ⌘1/⌘2 mode: "jump" (default, ⌘1–9 jump)
                                       #   or "cycle" (⌘1 next / ⌘2 prev; jumps shift to ⌘3–9)
density = "comfortable"                # optional; whole-app chrome sizing: "comfortable" (default)
                                       #   or "compact" (proportionally condensed type + spacing)
sidebar_drag = true                    # optional; default true — the sidebar chrome is a window-move
                                       #   drag handle (drag banner/empty list area to move; false = off)
auto_update = true                     # optional; default true — check for a new release on launch;
                                       #   false suppresses the auto-check (Check for Updates… menu still works)
probe = "…"                            # optional; session-presence probe, cascades global→window→tab
                                       # (cyan dot on exit 0; ghost on exit 3 = crashed but restorable)
probe_interval = 5                     # optional; slow-poll floor in seconds once a burst settles (default 5;
                                       #   0 = event-driven only — burst on triggers, then Idle, no steady poll)
kill  = "…"                            # optional; session-kill command, cascades global→window→tab; run fire-and-forget on confirm via the cyan dot
notify_debug = false                   # optional; default false — trace the notification path to $TMPDIR/warden-notify-dbg.log (per-user temp dir, e.g. /var/folders/…/T/; debug aid, read at launch; see docs/notifications.md)

[[window]]                             # = a native macOS window
title  = "work"                        # required, unique, non-empty; banner + window title
colour = "#0f8a8a"                     # optional; #rgb or #rrggbb; omit → neutral default
width  = 1500                          # optional; initial window width (px; default 1500)
height = 1000                          # optional; initial window height (px; default 1000)
open_on_start = true                   # optional; default true — materialize this window at launch.
                                       #   false = start closed-but-configured (open via the home surface / Window menu)
shell  = "zsh"                         # optional; per-window shell override
cmd    = "amux"                        # optional; per-window startup override
probe  = "…"                           # optional; session-presence probe override for this window
kill   = "…"                           # optional; session-kill command override for this window

  [[window.tab]]                       # = a project terminal (loose/ungrouped → headerless section, first)
  id         = "…"                     # optional; stable identity, needed only to disambiguate two tabs
                                       #   sharing a dir; empty = unset → identity falls back to dir
  title      = "alpha"                 # optional; default = basename(dir); a pure display label — non-empty
                                       #   if set, may repeat window-wide, renamable live
  dir        = "~/Developer/…/alpha"   # required
  shell      = "bash"                  # optional; per-tab shell override
  cmd        = "amux"                  # optional; per-tab startup override ("" = opt out → bare shell)
  load_on_open = true                  # optional; default false (spawn at launch + keep running for background work)
  probe      = "…"                     # optional; session-presence probe override for this tab ("" = opt out)
  kill       = "…"                     # optional; session-kill command override for this tab ("" = opt out)

  [[window.group]]                     # = a sidebar section (optional; loose tabs need no group)
  name = "backend"                     # required, non-empty, unique within the window

    [[window.group.tab]]               # same fields as [[window.tab]]; rendered under the group header
    dir = "~/Developer/…/api"

  [[window.root]]                      # = a scanned projects dir; each git repo found becomes a tab
  name  = "Developer"                  # optional; default = basename(dir); shares one uniqueness
                                       #   namespace with group names within the window
  dir   = "~/Developer"                # required
  depth = 6                            # optional; default DEFAULT_ROOT_DEPTH = 6; must be >= 1
  shell = "bash"                       # optional; per-root shell override for discovered projects
  cmd   = "amux"                       # optional; per-root startup override ("" = opt out)
  probe = "…"                          # optional; per-root probe override ("" = opt out)
  kill  = "…"                          # optional; per-root kill override ("" = opt out)
```

**`shell` and `cmd` cascade global → window → tab; the nearest set level wins.** A missing value at a level inherits from above; `shell` falls back to **your login shell** (`$SHELL`, run as a login shell — detected in `warden-app`/the CLI and injected via `resolve_with`, since the crate stays env-free; `DEFAULT_SHELL` is only the test/last-resort fallback) if unset everywhere; `cmd` is `None` (bare shell) if unset everywhere. An explicitly-empty `cmd = ""` counts as *set* and resets to `None`, so it opts a level out of an inherited command instead of inheriting it (see `cascade()` in `resolve.rs`). `shell` is treated the same (empty = unset). Resolution collapses the cascade into the flat `Tab.shell: String` + `Tab.startup: Option<String>` — the app never sees the levels.

**`[[window.root]]` introduces a third cascade shape: root → window → global, with no tab level.** A discovered project has no per-tab config of its own — the root it was found under is the nearest level, so `shell`/`cmd`/`probe`/`kill` cascade root→window→global exactly like the tab cascade (`""` still opts a level out), just one level shorter. `resolve_root` (`resolve.rs`) mirrors `resolve_tab`'s cascade minus the tab argument. This is unlike `[[window.group]]`, which adds **no** cascade level at all (a grouped tab still cascades tab→window→global) — a root and a group are structurally different: a group merely labels existing tabs, a root *is* the config level for tabs it hasn't seen yet.

**`probe` is a generic session-presence check.** It cascades global → window → tab exactly like `shell`/`cmd` (`""` opts a level out); resolution collapses to `Tab.probe: Option<String>`. warden runs it per tab via `sh -c` with cwd = the tab's dir, on a cadence the per-window scheduler sets (fast bursts on triggers, settling to the `probe_interval`-second floor or Idle — see `docs/probing.md`), substituting `{dir}`/`{title}`, and mapping the **exit code** to one of three states: **`0` ⇒ a
session exists** → a cyan dot; **`3` ⇒ no session, but a restorable one** → a **ghost**; **anything
else ⇒ nothing** → a hollow ring. The exit-3 leg is what stops a crashed-but-recoverable dir looking
identical to an empty one. Every non-3 failure collapses to *absent*, deliberately: a probe that
can't spawn, or that wedges past the timeout, must not ghost every tab. A probe that never exits 3
(a hand-rolled `tmux has-session`) simply never ghosts — the vocabulary degrades gracefully. The crate/app stay tmux/amux-agnostic — the command is opaque; the canonical amux case is `"$HOME/.agentmux/bin/amux" --probe` (exit 0 when the dir's session exists — the agent on tmux's default socket **OR** a lingering frame on the `agentmux-frame` socket). amux owns the session naming + socket layout, so the probe can't drift from it — the inverse of hand-inlining `tmux has-session` legs in warden config, which break silently when amux changes a socket name or its name sanitisation. The frame leg lights the dot when an agent has exited but its frame wrapper is still up — the state `amux --kill` reaps (no separate term check: a frame's scratch terminal never outlives the frame); a plain bare-amux (no frame) only ever has the agent session. Name amux by **absolute path** (a Finder launch's minimal PATH won't find a bare `amux`; see CLAUDE.md's probe-env footgun). Independent of the live/cold dot (a cold tab can have a running session).

The canonical amux probe reports the ghost too: `amux --probe` exits **3** when the dir has no live
session but `session_log.sh dropped --pending "$PWD"` is non-empty — i.e. when a plain `amux` launch
here *would* offer you its restore picker. `--pending` is the read-only twin of the `--new` gate the
picker itself uses; amux owns it precisely so warden's config doesn't have to know the ledger exists
(the same reason the probe delegates rather than inlining `tmux has-session` legs). The ghost
therefore means exactly one thing: **a plain launch here would offer a restore**.

**`kill` is a companion session-kill command.** It cascades and resolves identically to `probe` (`""` opts a level out → `Tab.kill: Option<String>`). warden runs it **fire-and-forget** via `sh -c` (cwd = tab dir; `{dir}`/`{title}` substituted raw — quote them if paths may contain spaces) when the user confirms via a two-step click on the cyan presence dot. Killing severs the session **without unloading warden's terminal surface** — the surface stays open at a live shell prompt after the session is gone. On confirm, `kill_session` runs the kill fire-and-forget then bumps the window so the scheduler fast-bursts; the chrome does **not** optimistically clear the dot — it tracks `warden:session-state` alone, staying lit through the brief teardown window and dropping once, with no flicker (see `docs/probing.md` and CLAUDE.md's kill-flicker footgun). A genuinely-failed kill leaves the session present, so the dot correctly stays lit. The arm-state (the pending confirm) lives in the chrome only — no IPC until confirm. Because the kill control lives **on** the presence dot, `kill` is only reachable on tabs that also set `probe` — no `probe` ⇒ no cyan dot ⇒ no way to trigger the kill (the command still resolves, it just renders no affordance). This coupling is by design (documented, not enforced): a tab inheriting a global `kill` while opting out of `probe` has a silently inert kill. `kill` inherits the same probe PATH/env footgun (now half-closed by `restore_login_path` — see CLAUDE.md's probe-env footgun): warden imports the login-shell PATH at startup so a packaged `warden.app` resolves bare Homebrew binaries, but absolute paths stay the robust fallback — warden imports PATH only, not your shell exports. The canonical kill is `"$HOME/.agentmux/bin/amux" --kill`, the mirror of `amux --probe`: with cwd = the tab dir it tears down the **whole project** — the agent session on the default socket plus its `-frame`/`-term` sessions on the dedicated sockets — so it reaps exactly what the probe detects. (`amux --kill` itself shells bare `tmux`; same login-PATH resolution.) The example config delegates to it rather than a raw `tmux kill-session`, which would kill only the agent and orphan the frame/term.

The kill affordance is bound to the **cyan** dot only — a ghost is a drop on a *dead* server, so
there is nothing to kill, and chrome-core never renders a kill on it.

**Restarting a dead session reuses `cmd` — no new config field.** When a probe reports the session **absent** on a **live** tab, the same cyan dot becomes a single-click **start** control (the non-destructive mirror of `kill`'s two-step confirm). It types the tab's resolved `cmd` (`Tab.startup`) into the **existing live shell** and submits it — `TerminalSurface::run_command` (ghostty impl behind the seam) injects the command text via `ghostty_surface_text`, then a **real Enter key event** (`ghostty_surface_key`, `kVK_Return`), the same path `forward_key` uses for a physical Return. This is the runtime twin of how the tab launched: it preserves the terminal/scrollback (no respawn) and, being typed into an interactive shell, resolves a shell *function* like `amux`. **Footgun — a trailing `"\n"` in the injected text does NOT run the command** (the exact bug this shipped with first): `ghostty_surface_text` lands like a *paste*, so a shell in bracketed-paste mode inserts the newline literally, and a bare LF isn't the Enter byte a shell's line-editor accept-lines on anyway — the command is typed but never executed. Submitting **must** be a synthesized Enter *key event*, not a newline character; don't "simplify" `run_command` back to appending `\n` to the text. **Also do NOT "fix" this to run `cmd` via `sh -c` like `probe`/`kill`** — the probe/kill symmetry tempts it, but a detached `sh -c` wouldn't attach to the tab's terminal (and couldn't run a shell function); start is fundamentally an *into-the-live-surface* action, not an external command. Gated on all of: a `probe` (so "absent" is meaningful), a `cmd` (something to send), a **live** surface (a shell to type into), and an absent session — the chrome computes this in `presenceClass`/the `startable` DTO flag (`registry.rs` `has_cmd`). A cold tab shows no start affordance: activating the row lazy-spawns + runs `cmd` already. Unlike the synchronous `kill` (a single bump), the started session appears **asynchronously** (the shell has to run the command), so `start_session` (`main.rs`) bumps the window immediately and again at ~1s/~3s so the scheduler's fast burst re-arms and catches the session once it lands rather than settling "absent" first; the slow floor heals any later drift, and `probe_interval = 0` tabs re-light on the next event. Same login-PATH/env inheritance as probes.

**The ghost is pure decoration on this same start affordance** — no separate "recover" command, no
new config field. A ghosted tab is a non-present session, so `startable` already holds; clicking it
types the same resolved `cmd`, and amux's auto-picker (which fires on a plain launch) offers the
restore. Declining is the dismissal: the picker's `--new` read burns the once-per-(dead-server, cwd)
marker either way, so `--pending` goes empty and the ghost clears within a probe interval whether you
restored or not. A **cold** ghosted tab is informational only (no live shell to type into) —
activating the row lazy-spawns and runs `cmd`, firing the same picker.

**`cmd` runs inside the shell, it doesn't replace it.** Every tab spawns its resolved `shell` (an interactive shell under the PTY); the resolved `cmd`, if any, is delivered as libghostty `initial_input` — i.e. *typed into* that shell (newline-terminated) rather than exec'd directly. This is deliberate and load-bearing: `amux` is a shell **function**, not an executable, so execing it directly fails — only an interactive shell resolves it. As a bonus the shell stays live after the command exits (detaching from `amux`/tmux drops you to a prompt, not a dead pane). Do **not** "fix" this back to passing `cmd` as libghostty's `command`/exec target.

**Groups are presentation only — a sidebar sectioning concern, not a new container of sessions.** `[[window.group]]` blocks hold `[[window.group.tab]]`s; resolution **flattens** loose tabs + every group's tabs into the one ordered `Tab` list (loose first, then groups in file order), tagging each with `Tab.group: Option<String>` (`None` = loose). Crucially, **the flat `Tab` list is the only downstream shape** — `reconcile`, the registry, tab navigation (⌘1–9 / cycle), the live/cold dot, unload, and notifications all stay flat and key-based; only the chrome re-sections by `group` at render time. Groups add **no cascade level** (a grouped tab cascades tab → window → global exactly like a loose one). A kept tab's `group`/`probe`/`kill` change **is** detected by `reconcile` (via `WindowUpdate.set_meta`) and applied live — sidebar re-section for `group`, new probe/kill commands picked up on the next poll/kill — without respawning the PTY, the same live-metadata path a `title` relabel takes (see CLAUDE.md's tab-identity footgun). A kept tab's terminal-spec fields (`dir`/`shell`/`cmd`/`load_on_open`) take the sibling `respawn_tabs` path instead: same live, no-restart-the-app apply, but that one tab's PTY is torn down and respawned in place since a terminal can't migrate its cwd/session. This nested-authoring → flat-label shape matches how curator itself models groups (it flattens `[[group]]` to a per-tab label too).

**Project-tree roots reuse group-sectioning rather than adding a new downstream shape.** The `warden-config` crate stays pure — a `[[window.root]]` is only a declaration (`Root`); no scanning happens in the crate. `warden-app/src/scanner.rs` walks a root's `dir` (stopping at every `.git` dir/file — never descending into a repo — skipping hidden dirs and symlinks, depth-limited by `Root.depth`, deterministic/sorted order) and `synthesize_tabs` turns each discovered project into a `Tab` with `group = root.name` (so the existing group-sectioning renders it under a labelled section) and the root's cascaded `shell`/`cmd`/`probe`/`kill`. `manager::effective_config` appends every window's synthesized tabs to `window.tabs`, producing the **effective config**; `WindowManager` keeps `raw_config` (unexpanded, for re-scanning) alongside `last_good` (effective) — everything downstream (`window_to_spec`, `reconcile`, the registry) consumes only the effective config, so a discovered project needs no special-cased path. `plan.rs`'s `derive_tree_meta` — used by both `window_to_spec` and `reconcile_ops` so hot-reload/rescan-added tabs land correctly too — looks up a tab's `group` against the window's root dirs to derive `TabSpec.tree` (is this a tree row) and `tree_path` (folder segments between the root dir and the project, via `scanner::tree_path`), carried to the chrome as `TabDto.tree`/`treePath`. Rescanning happens on window open, on every config hot-reload, and via a manual refresh control in the root's sidebar section header (chrome `onRescan` → the `rescan_root` command) — **there is no live filesystem watcher for roots** (deferred; see `docs/FOLLOWUPS.md`).

**Curated and discovered tabs share one dir-keyed identity scheme, so a curated tab shadows a same-dir discovered project.** A curated tab's `Tab.key` is its explicit `id` if set, else its normalized `dir` (`normalize_dir_key` in `resolve.rs` — the lossy path with any single trailing separator stripped, except a bare root `/`); a discovered `[[window.root]]` project's key is its absolute path (`scanner::synthesize_tabs`, `path.to_string_lossy()`, never trailing-slashed) — the same normalized form, deliberately, so the two schemes line up. Two projects sharing only a basename (e.g. `~/Developer/a/api` and `~/Developer/b/api`) stay distinct rows since the full path is the key. A curated tab placed at the same `dir` as a project a `[[window.root]]` would otherwise discover **shadows** it: `manager::effective_config` appends synthesized tabs after curated ones and dedups by key keeping the first occurrence, so the curated tab wins and the discovered one never renders — a deliberate way to hand-curate one project inside an otherwise-scanned tree. `reconcile` diffs every tab by `Tab::key` uniformly regardless of origin, so a discovered project needs no special-casing in `add_tabs`/`remove_tabs`/`set_meta`/`respawn_tabs`. Title carries **no** uniqueness constraint for either kind — see CLAUDE.md's tab-identity footgun.

**`density` is a whole-app chrome-sizing mode** — `"comfortable"` (default) or `"compact"`. It resolves to `Config.density: Density` (global, no cascade; unknown value → `ResolveError`, like `tab_digit_keys`) and is carried per-window in the chrome DTO (`InitDto.density`, on init and hot-reload refresh — `apply()` takes the new density so a live flip restyles without a relaunch). warden resolves `density` and passes it in the DTO; **chrome-core** applies `data-density` on `<html>`, swapping its `--cc-*` sizing tokens (`--cc-row-font`, `--cc-tile-size`, `--cc-dot-size`, `--cc-sidebar-w`, paddings/gaps); `compact` is a proportional ~0.85× scale of the comfortable set. Tweak the scale in one place — chrome-core's `assets/sidebar.css` (`:root` / `[data-density="compact"]`); the sidebar's default + double-press-reset width reads `--cc-sidebar-w` so it follows density too. **All three apps consume chrome-core**, so the tokens live once and stay aligned by construction.

Validation: unique window title, unique tab **identity** (`id`-else-normalized-`dir`) within a window (window-wide — across loose tabs and all groups, curated tabs only; two curated tabs sharing a `dir` with no disambiguating `id` → `DuplicateTabIdentity` — see CLAUDE.md's tab-identity footgun; `title` carries no uniqueness constraint), non-empty title/dir/explicit-title, non-empty root dir/name, root `depth >= 1`, malformed colour → **errors**; width/height ≤ 0 → **error**; section-name uniqueness (group names + root names) is one shared namespace **window-wide**: a group-vs-group clash is `DuplicateGroup`, any other clash (root-vs-root or a root sharing a group's name) is `DuplicateSection`; a `dir` that doesn't exist (tab or root) → **warning** (tab/root still created, so a root can point at a not-yet-cloned dir without erroring). Invalid config must be reported, never panic.
