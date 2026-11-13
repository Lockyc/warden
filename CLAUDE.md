---
type: architecture
links:
  - rel: see-also
    to: README.md
    note: README is the user-facing tour; this is the agent-facing architecture doc
---

# warden — agent orientation

## What warden is

warden is a **config-driven terminal multiplexer** — "curator for terminals." A single TOML file is the source of truth: it defines **windows** and the **project tabs** inside them. The app materializes itself from that config and **hot-reloads on save**. Each window carries a colour + title banner; each tab is a real terminal opened in a working directory running an optional command. warden is **generic and content-agnostic** — the command a tab runs is arbitrary.

Target platform: **macOS**. Linux is a possible future consideration, not a guarantee — `warden-config` stays platform-neutral to keep that door open. Windows is unaddressed, not ruled out: a port needs a new per-OS shim behind the `TerminalSurface` seam.

## Reading order

| Before working on… | Read |
|---|---|
| config parsing, resolution, cascade, validation | [`docs/config.md`](docs/config.md) |
| the libghostty embed — surfaces, FFI, focus/keys, links, rendering, revendoring | [`docs/surface.md`](docs/surface.md) |
| splits, pop-out, the pane chrome | [`docs/native-splits-direction.md`](docs/native-splits-direction.md) |
| Rex-backed tabs — the hub, `warden-relay`, `rex-client`, size ownership, the session lifecycle | [`docs/rex.md`](docs/rex.md) |
| the presence dot / probe scheduler | [`docs/probing.md`](docs/probing.md) |
| notifications, badges, banners | [`docs/notifications.md`](docs/notifications.md) |
| packaging, signing, releases, the updater, `install.sh` | [`docs/releasing.md`](docs/releasing.md) |
| anything that looks missing | [`docs/FOLLOWUPS.md`](docs/FOLLOWUPS.md) — it may be a conscious deferral |

The README is the user-facing feature tour; this file doesn't repeat it.

## Current state

Four crates, all built:

- **`warden-config`** — pure logic, no GUI: parse/validate/resolve the TOML, diff two configs for hot-reload, load, watch, and the `warden validate` + `warden fmt` CLI.
- **`rex-client`** — a blocking Rex protocol client (control + data connections) and, behind its `fake` feature, the fake server the gate tests run against.
- **`warden-relay`** — the helper binary a Rex pane runs: one Rex block streamed to its PTY, input sent back. Links libghostty-vt, never GhosttyKit; bundled into `Contents/MacOS/`.
- **`warden-app`** — the macOS-only Tauri app. Implementation anchors, not a feature list:
  - **Windows**: one native window per `[[window]]` (`open_on_start`-gated), curator-style transparent chrome. Which are open and under which label is shell-core's `WindowSet`, which builds and tears down each window's terminals through `windows.rs::WardenHost`; New / Edit / Delete Window… (Window menu, banner right-click) are its form flow. Bounds persist per launch label via shell-core's geometry plugin; sidebar width in `localStorage`.
  - **Tabs**: libghostty surfaces behind `TerminalSurface`, lazy-spawned on first focus except `load_on_open` and, with `remember_tabs`, the window's last-loaded tabs (`session.rs`). `[[window.group]]` sections; `[[window.root]]` trees discovered by `scanner.rs::synthesize_tabs` over config-core's `discover_projects` (rescan on open / hot-reload / manual refresh — no fs watcher). `open_tabs_section` / `background_tabs_section` opt into chrome-core's pinned Open / Background sections.
  - **Tab-row dots**: live/cold doubling as unload (`Registry::unload`; unloading the visible tab leans up to a live neighbour); amber attention from an unread bell / OSC 9 / OSC 777 (`action_cb` → `notify.rs`) or from an agent there needing you (its list, `rex/attention.rs::needs_you`; `tabAttention` in `ui/index.html`) — banners via shell-core's `banner`, clicking one raises the window + selects the tab; three-state presence from the per-tab `probe` (cyan = exit 0, ghost = exit 3 restorable, hollow), doubling as end-session (☠ `kill` / ⏻ `suspend`, `main.rs::end_session`) or start (types `cmd`).
  - **Empty states**: warden is persistent (`quit_on_last_close(false)`: closing the last window shows shell-core's home surface; ⌘Q quits, and so does closing the home surface with nothing else open — the Builder's `on_window_event`). The `WindowSet` owns the home surface (`NoConfig` / `Broken` / `Windows`); warden tells it the load error and the pop-out count. A mid-edit parse error with windows open shows an error banner instead.
  - **Menu**: App/Edit/Config/Window are shell-core's spine (`menu::build_spine`) and the whole Tab submenu is `menu::build_tab_menu` (navigation, the Cycle Tabs with ⌘1/⌘2 toggle that writes `tab_digit_keys`, Close / Pop Out / Find), with warden's items as its groups — Split ⌘D, New Scratch Tab ⌃⌘T (a Rex column's); Suspend ⌘⇧S / Terminate ⌘⇧K via chrome-core's `requestEnd`; the agent and Sleep Server items. `menu::route` picks the window a spine command acts on (a pop-out's origin for the sidebar commands; ⌘W in a pop-out's main pane docks it); `main.rs::on_spine_action` emits the label-stamped events. Edit's ⌘C / ⌘V / ⌘A take first refusal from a focused terminal too, and arrive at its view's `copy:` / `paste:` / `selectAll:`, which run libghostty's own bindings (`surface/ghostty.rs`). Reopen Last Closed (⌘⇧T) is spliced into the Window submenu. ⌘W, from either pane, closes a selected agent tab ("Close Agent Tab", `main.rs::agent_closable`), else unloads the tab (a pop-out's docks), and ⌥⌘W always unloads it (disabled in a pop-out); a scratch pane closes only from its right-click Close Pane (`Registry::close_pane_item`), `exit` or the strip's ×. A terminal never sees ⌘W, even disabled (`surface/ghostty.rs::is_close_chord`): libghostty's own binding would end the pane. ⌘T adds an agent tab from either pane; ⌃⌘T adds a scratch tab from either pane. ⌘D is retitled Split Above in a scratch column (`pane_menu::split_title`) and disabled with the reason where it can't split (a pop-out among them, as are Suspend / Terminate there). In a tab with agent tabs (from any of its panes) ⌥⌘S sleeps an agent tab and ⌘⇧T reopens one, and ⌘↑ / ⌘↓ (⌘K / ⌘J through hidden alias items, `main.rs::hide_alias_items`), ⌥⇧⌘↑ / ⌥⇧⌘↓ and ⌥⌘T step, move and fork (`rex::lifecycle::agent_step` / `agent_move`); ⌥⌘← / ⌥⌘→ cross a split, docked or popped out (`Registry::pane_to`, `DetachedSurface::pane_to`). Their enabled state follows the focused window's active tab, and the pane items its focused pane; Pop Out Tab is disabled and retitled with the reason while a column is too full to carry or still opening (`main.rs::sync_agent_menu`, re-run on window and pane focus, activation and list changes — no rebuild); a disabled item leaves its chord to the terminal, ⌘W aside. Tab ▸ Sleep Server lists one item per Rex server, disabled with its reason beside its name (`main.rs::sync_server_menu`, re-run on the hub's connection events and window focus; all disabled while the home surface is focused, and a pop-out's acts in its origin). Composition: `main.rs::build_app_menu`. A terminal's right-click menu is `pane_menu.rs`'s items, popped by the `pane_menu` command once the page answers `warden:pane-menu` with the split's live ratio and a block's share of its run (a pop-out's by `manager.rs::handle_context_menu` itself).
  - **Agent tab list**: `#agents` in `ui/index.html`, a column left of a Rex tab's main pane (`TabDto.agents`), outside the reported primary rect; it renders `warden:agents` / `agents_init` and calls the `agent_*` commands; its width is `--agents-w`, dragged from `#agents-grip`. Each row's second line holds its current step while working, else what it needs from you, else what its last reply did (`renderAgents`), then the background work and the prompt cache (`paintCache`). Above its rows, `#agents-head`: the tally (`renderTally`), `+` and the fold, ending level with the sidebar's Search box (`syncAgentsHead`). Under the main pane, `#summary`: the selected agent's title / asked (→ a short reply) / doing / said (`AgentRowDto`; sources in [`docs/rex.md`](docs/rex.md)); beside it at the list's foot, `#agents-tile`: the tab title, server and session. Both in `TabDto.colour` (`project_colour.rs`; a tab's `colour` pins it).
  - **Splits and pop-out**: a primary + optional scratch column per tab (config `split` or ⌘D) — one pane on a plain tab; on a Rex tab one Rex session of scratch tabs (`#scratch-tabs`) and stacked blocks (`Registry`'s `Column`, `PaneIdx::Block`) — and whole-tab, session-preserving pop-out (`GhosttySurface::reparent`, `Registry::detach`/`attach`). Mechanism and traps: [`docs/native-splits-direction.md`](docs/native-splits-direction.md).
  - **Rex tabs**: a tab with a `server` keeps each pane in a Rex session; its surface runs `warden-relay`. `rex/`: `RexHub` (one control connection per server), `store.rs` (pane key → session), `spawn.rs` (off-main-thread open), `size.rs` (size claims), `unclaimed.rs` (`Rex · <server>` rows), `windows.rs` (each session's windows and block statuses), `layout.rs` (a window's split tree as tiles and dividers; a config stack as a new session's layout), `column.rs` (the split pane's scratch column session), `reports.rs` (the relays' resume-report socket), `agents.rs` (agent records + the resume-shape allowlist), `agent_open.rs` (a main pane's open; only a user open wakes), `lifecycle.rs` (the agent tab list's ops, Sleep Server), `attention.rs` (which agents need you; banners). Unload detaches, ☠ destroys, a program exit destroys a session it was the last block of; the dot comes from Rex, not `probe`. [`docs/rex.md`](docs/rex.md).

Deferred: ad-hoc `⌘T`/`⌘N` tabs, seam/FFI/IPC hardening, watcher debounce, argv passthrough — see [`docs/FOLLOWUPS.md`](docs/FOLLOWUPS.md).

## Intended architecture

- **Tauri**, one app, one dock icon; per-window identity is the in-app banner, not separate bundles.
- **Windows = native macOS windows** (`cmd+\`` cycles them); **tabs = projects** within a window.
- **Terminal = embedded libghostty** behind the `TerminalSurface` seam — a per-OS native shim (NSView today). Its embedding C API is unstable, so it is pinned (see [`docs/surface.md`](docs/surface.md)). A non-libghostty fallback could sit behind the same seam.
- **Splits are warden's to grow, not a multiplexer's to provide.** Direction and what it unlocks: [`docs/native-splits-direction.md`](docs/native-splits-direction.md). The Superlogical research lives once, in agentmux's `docs/multiplexer-direction.md` — read it before researching that from the web, and append news there.

## Workspace layout

`crates/warden-config` — library + `warden` CLI. Data flows one way:

```
raw.rs       serde structs mirroring the TOML schema + parse()
  ↓
resolve.rs   validate + fill defaults + expand ~ → (Config, Vec<Warning>); ResolveError
  ↓
model.rs     resolved types: Config / Window / Tab / Warning
reconcile.rs reconcile(old, new) → Reconciliation (open/close windows; per-window colour, tab add/remove/reorder, set_meta relabel, respawn_tabs, split_tabs/unsplit_tabs)
load.rs      load(path); config_path() (WARDEN_CONFIG else ~/.config/warden/config.toml); LoadError
watch.rs     Watcher — parent-dir notify watcher; fires load() on change
bin/warden.rs  `warden validate` (warden's schema) + `warden fmt` (delegates to config-core's shared fmt_cli)
```

Formatting and colour live in config-core, re-exported at the crate root (`format_str`/`format_file`, `Colour`).

`crates/warden-app` — `plan.rs` (config → `WindowDecl`/`WindowSpec`/`TabPlan`, reconcile → `TabOp`; `derive_tree_meta`), `manager.rs` (`WindowManager`: each window's terminals, held configs, pop-outs; `effective_config` expanding roots), `windows.rs` (`WardenHost`), `scanner.rs`, `registry.rs` (per-window tab registry over surfaces), `surface/` + `ffi/` (the libghostty embed), `geometry.rs` (web-rect ↔ NSView-rect), `probe.rs`, `notify.rs`, `session.rs`, `rex/` (Rex tabs). `ui/index.html` is the page shell + controller; `ui/scratch-run.js` the scratch column's run maths, a classic script with `node:test` tests in `tests/scratch-run.test.js`; the home surface is shell-core's `home.html`.

`crates/rex-client` — `control.rs` (`ControlClient`), `data.rs` (`DataStream`), `wire.rs` (codecs, `StreamDemux`, `strip_terminal_replies`), `endpoint.rs`, `fake.rs`. `crates/warden-relay` — `main.rs` (stream + stdin loops, resume/restart, redraw on a size change), `vt.rs` (libghostty-vt FFI: snapshot → VT), `term.rs` (raw mode, SIGWINCH count); vendored libghostty-vt in its `vendor/`.

## Config schema (`~/.config/warden/config.toml`; override with `WARDEN_CONFIG`)

**Full reference in [`docs/config.md`](docs/config.md) — read it before touching parsing/resolution.** The README's *Every option* tables are the user-facing index of every key, so **a new key lands in `docs/config.md` and the README tables in the same change as the parser**. Load-bearing invariants:

- **`cmd` is typed *into* the shell, not exec'd** — it's libghostty `initial_input`, so a shell *function* like `amux` resolves and the shell survives the command exiting. Don't pass it as libghostty's `command`.
- **Tab identity is `id`-else-normalized-`dir`, never the title** (`Tab::key`); `title` is a repeatable display label. Curated and discovered tabs share the scheme, so a curated tab shadows a same-dir discovered project.
- **Groups add no cascade level; roots add one** (root → window → global). Both flatten to the single ordered `Tab` list with a `group` tag — the only downstream shape.
- **Resolution collapses the cascade** (`shell`/`cmd`/`probe`/`kill`/`suspend`/`split`, nearest level wins, `""` / `split = false` opts a level out); the app never sees levels. `density`/`tab_digit_keys` are global-only; `open_tabs_section` and `background_tabs_section` cascade global → window only, so they reach live windows as an ordinary `WindowUpdate`.
- **Validation reports, never panics**; a missing `dir` is a warning, and the tab still opens.

## Build / test / run

- `cargo build` / `cargo test`; `warden-app` compiles only on macOS, by design.
- **`just`** lists recipes: `just run` (against `examples/config.toml` — never your real config), `validate`, `test`, `fmt`, `clippy`, `gate` (fmt-check + clippy + Rust and JS tests + config fmt-check + active-`[patch]` guard), `build`, `install`.
- **`just hooks` once per clone** — `core.hooksPath` is local config, so a fresh clone has neither the docgraph pre-push gate nor the active-`[patch]` pre-commit guard until it runs. No CI: warden ships on macOS only, so the local `just gate` is the check — green per change and before tagging a release.
- **Toolchain is pinned** in `rust-toolchain.toml`; config-core carries the canonical pin — bump there first, then here and in curator and lector. The cores build with warden's toolchain, not their own.
- Vendored libghostty and revendoring: [`docs/surface.md`](docs/surface.md). Packaging, signing, releases, the updater: [`docs/releasing.md`](docs/releasing.md).

## Conventions & footguns

**The normal case is boring.** A TOML edit becomes windows + tabs; saving hot-reloads them through a straight config → materialize → reconcile pipeline. Almost everything below is a GUI-launch or native-embedding edge — real traps, but rare ones. Subsystem traps live in the docs in the reading-order table; this section holds the rest.

**Two premises the entries lean on:**

- **Minimal GUI PATH.** A `.app` opened from Dock/Finder/Spotlight inherits only `/usr/bin:/bin:/usr/sbin:/sbin`, so a bare-name shell/probe/kill command fails unless PATH is restored or the command is absolute.
- **A title edited in the file is destructive.** It is close(old) + open(new): its terminals and its persisted per-label bounds are recreated. Edit Window… renames in place instead.

### Cross-cutting rules

- **No transitional fallbacks.** Finish every migration in-branch.
- **Keep tool-specific concepts out of `warden-config`** (neutral test strings). The README leads with Rex tabs and agents as the workflow, with plain tabs documented alongside. It doesn't name agentmux: Rex tabs replace it.
- **Raw-string TOML fixtures with a colour hex** self-terminate at `"#` inside `r#"…"#` — use `r##"…"##`.
- **NEVER move the user's real config aside to test `NoConfig`/`Broken`** — `WARDEN_CONFIG="$(mktemp -d)/nonexistent.toml"` reproduces `NoConfig` with nothing to restore. Moving it has destroyed a config once.
- **`open --env "WARDEN_CONFIG=<path>" -a /Applications/warden.app` is silently ignored if warden is already running** — and warden is persistent, so quit it first (`osascript -e 'quit app "warden"'`).
- **"No notifications at all" (not even the badge) is upstream transport, not warden.** The producer must wrap OSC 9/777 once per nested tmux layer (agentmux ≥ 0.14.0 does under `--frame`). Diagnose at the agent→frame boundary: `tmux -L agentmux-frame pipe-pane -t <agent-pane> 'cat >> /tmp/cap'` — a bare `]777` = dying at the frame; a `Ptmux;` envelope = it reaches warden.
- **Badge but no banner on macOS 26 is the OS, not `notify.rs`** — don't rewrite it. Discriminator and fixes: [`docs/notifications.md`](docs/notifications.md).
- **A link that won't open, with no alert shown, is the tmux hop or mouse reporting** — check order in [`docs/surface.md`](docs/surface.md). An alert is `surface/link.rs` screening the target.
- **warden-app logs through `log::{error,warn,info,debug}!`, never `eprintln!`** — an installed app's stderr is `/dev/null`. The backend is `logging.rs` (unified log, subsystem `au.lsjc.warden`); `info!` is persisted, `debug!` is live-stream only. Read it: `log stream --predicate 'subsystem == "au.lsjc.warden"' --level debug`, or `log show --last 1h --predicate …`. `warden-relay` keeps `eprintln!` — its stderr is the pane.
- **A mousedown drags only if the element under the cursor carries `data-tauri-drag-region`** — children don't inherit it. chrome-core owns `#cc-titlebar`; warden's `#sidebar` carries the attribute.

### Call-site pointers (the named code comment carries the why)

- **Install launches via shell-core's `scripts/launch-app.sh`, never a bare `open`**, which forwards the installing terminal's env (`TERM_PROGRAM`, `GHOSTTY_*`, `SHELL`, …) into every tab. Symptom: "misbehaves from `just install`, fine from Spotlight."
- **Default shell = `$SHELL -l`, absolute** (`warden_config::login_shell`, shared with the `warden` CLI); config shells may be bare, covered by **`restore_login_path`**, which imports the login-shell PATH at startup (best-effort, 3s deadline). Don't drop either expecting the other to cover it.
- **Probes inherit warden-app's env** — PATH is restored, shell exports aren't, so a probe names its tool absolutely (agentmux's: `"$HOME/.agentmux/bin/amux" --probe`). `probe.rs`.
- **The probe child's `$PWD` is the configured `dir`**, set explicitly in `probe.rs::tab_command` (probe, kill and suspend alike) — a `$PWD`-keyed probe otherwise misses its session under a symlinked path.
- **The probe scheduler is the single driver** — don't serialize the sweep, drop the QoS demotion, batch the cache write, or add a one-shot reprobe. [`docs/probing.md`](docs/probing.md).
- **Never add an optimistic dot-clear on kill** (off→on→off flicker): the dot tracks `warden:session-state` alone; the command completes before a directional `bump_tab_await(false)`. `main.rs::end_session`.
- **`emit_to` leaks to sibling webviews** — every per-window event stamps a `label` and the chrome filters with `forMe()` (`main.rs` `on_menu_event`, `manager.rs` `warden:refresh`, `notify.rs`, `probe.rs`). The exception is `warden:agents`: one app-wide emit keyed by `tabId`, since the list is the same in every window, and each window keeps only its own tabs' lists (`lifecycle.rs`, `ui/index.html` `agentLists`).
- **A new *global* setting won't hot-reload if only threaded through the per-window DTO** — a global-only edit is an empty reconcile, so add an old-vs-new compare in `main.rs`'s reload hook (launch works via `InitDto`, which hides it).
- **A hot-reload while the home surface lists windows must reconcile, not re-materialize** — key recovery on `last_good.windows.is_empty()`, not on zero windows open (`main.rs` reload hook).
- **Every load failure routes through `WindowSet::set_load_error`** — never a direct `show_home`/`close_home` or a bespoke error window (`main.rs`).
- **Never call a `WindowSet` method that re-enters `WardenHost` while holding `ManagerState`** (`materialize`, `apply`, `open_or_focus`, `rename`, `delete`) — the host takes that non-reentrant lock (`main.rs::materialize_effective`, `redock`). Lookups and `set_detached_count` are fine under it.
- **Format-on-save rewrites only on the clean-parse branch and only if bytes differ**, atomically; formatting stays idempotent so warden's own write settles after one retrigger (reload hook + config-core `format_file`).
- **The watcher matches the config by file name and fires on any event** (`warden-config/src/watch.rs`) — don't restore full-path equality or add an `event.kind` filter.
- **`rescan_root` diffs a fresh scan against `last_good`** (what's on screen), not two scans (`main.rs::rescan_root`).
- **Window bounds restore is the geometry plugin's own `on_window_ready` hook** — add no manual trigger (FOOTGUN in `manager.rs::build_window`).
- **A surface that fails to spawn degrades to a cold tab — never `.expect()`/`unwrap()`**; errors ride the init snapshot or `warden:error` (`registry.rs` `add`/`ensure_spawned`/`activate`).
- **Tab reconcile shapes** — which kept-tab edits are a live `set_meta` and which a `respawn_tabs`, and why a keyless `dir` change is remove + add: listed once, on `reconcile.rs::reconcile`.
- **Stale frontend embed:** `ui/` embeds at compile time; `build.rs` watches `ui/index.html` (add any new hand-written page, but don't watch `ui/` broadly — build.rs writes there). "My HTML change didn't take" → touch a `.rs`.
- **The empty-state placeholder is opaque and composited behind the content** (`#empty-state`); it lives in warden, not chrome-core.
- **Icon safe area** (824×824 on the 1024 canvas) is enforced by `plate icon`, which `assets/build-icons.sh` calls — design the SVG edge-to-edge.

## Shared cores: chrome-core and shell-core

warden, [curator](https://github.com/Lockyc/curator) (browser keeper-tabs) and [lector](https://github.com/Lockyc/lector) (local doc sites) share their sidebar and Tauri setup through git-rev-pinned cores. Change shared behaviour in the core, never here.

**[chrome-core](https://github.com/Lockyc/chrome-core) is the sidebar** — banner, tab rows and their dots, confirm row, density, resize, error bar, project tree, the Open and Background sections, and the self-updater. It's a build-dependency: `build.rs` writes `ui/chrome-core.{css,js}` (git-ignored) — edit its `assets/sidebar.{css,js}`, never the copies. `ui/index.html` is the page shell (`#terminal-hole`, `reportRect`, `forMe`, the first-probe handshake) plus a controller mapping the component's callbacks to warden commands and warden events to component calls.

- **A new `TabDto` field is invisible to the chrome until `toComponentDto` forwards it.** The Rust DTO and the component DTO are separate shapes hand-mapped in `ui/index.html`; add a field to both in the same change (this is how `tree`/`treePath` once rendered flat).
- **A page-level function name can replace chrome-core's own.** `chrome-core.js` is a classic script, so its top-level `function`s share the page's global scope; a `function el` here replaces chrome-core's `el` and the sidebar renders as bare, unstyled elements. The agent list's helpers carry an `agent` prefix for that reason.
- warden-only DTO fields: `presence`, `killable`, `suspendable`, `startable`, `tree`/`treePath`; window-level `windowDrag` (`sidebar_drag`), `openSection` (`open_tabs_section`), and `backgroundSection` (`background_tabs_section`). warden passes **no** `active`, so the component owns selection.
- **Dev loop:** `just chrome-dev` / `just chrome-pin` (likewise `config-*` and `shell-*`; scoped `#PATCH:<core>#` sentinels in the workspace `Cargo.toml`). **Never commit an active patch** — `just gate` and the pre-commit hook refuse. Visual tweaks: `just chrome-preview`.

**[shell-core](https://github.com/Lockyc/shell-core) is the byte-identical Tauri sliver** — release scripts (`materialize_scripts` → git-ignored `scripts/*.sh`, configured by the tracked `scripts/tooling.env`), the build stamp (`build_stamp()` → `BUILD_GIT_SHA`/`BUILD_DATE` in About), the menu spine and tab nav, the home surface, the detach window shell, and plugin registration incl. geometry persistence (`register_plugins(builder, Some(&config_path), &[])`). Its `build.rs` build-dep is `default-features = false` so it stays tauri-free; the normal dep enables `runtime`.

- **shell-core never touches config-core** — the home surface's "Create a starter config" is warden's own `shell_home_create_config`, calling `config_core::write_default_config` with `src/default-config.toml`.
- **`DetachSpec.panes` is a generic `Vec<f64>` of hole ratios**, not a warden split type — an empty vec keeps curator's and lector's payload unchanged.
- **Stays warden's own:** IPC fan-out (`emit_to` + `forMe()`), the config watcher (parses inside, drives root scans; curator/lector share `shell_core::watch`), and the warden-only Tab items. No chrome-caller command gate — warden's surfaces are native views with no webview to spoof a call.
- **`warden://open?window=<title>`** (`Info.plist` `CFBundleURLTypes` → `RunEvent::Opened` → shell-core's `open_request` → `main.rs::open_or_focus_window`, the one open-by-label path).
