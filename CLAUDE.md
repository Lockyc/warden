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
| the presence dot / probe scheduler | [`docs/probing.md`](docs/probing.md) |
| notifications, badges, banners | [`docs/notifications.md`](docs/notifications.md) |
| packaging, signing, releases, the updater, `install.sh` | [`docs/releasing.md`](docs/releasing.md) |
| anything that looks missing | [`docs/FOLLOWUPS.md`](docs/FOLLOWUPS.md) — it may be a conscious deferral |

The README is the user-facing feature tour; this file doesn't repeat it.

## Current state

Two crates, both built:

- **`warden-config`** — pure logic, no GUI: parse/validate/resolve the TOML, diff two configs for hot-reload, load, watch, and the `warden validate` + `warden fmt` CLI.
- **`warden-app`** — the macOS-only Tauri app. Implementation anchors, not a feature list:
  - **Windows**: one native window per `[[window]]` (`open_on_start`-gated), curator-style transparent chrome. Bounds persist per Tauri label via shell-core's geometry plugin; sidebar width in `localStorage`.
  - **Tabs**: libghostty surfaces behind `TerminalSurface`, lazy-spawned on first focus except `load_on_open` and, with `remember_tabs`, the window's last-loaded tabs (`session.rs`). `[[window.group]]` sections; `[[window.root]]` trees discovered by `scanner.rs::synthesize_tabs` over config-core's `discover_projects` (rescan on open / hot-reload / manual refresh — no fs watcher). `open_tabs_section` opts into chrome-core's pinned Open section.
  - **Tab-row dots**: live/cold doubling as unload (`Registry::unload`; unloading the visible tab leans up to a live neighbour); amber attention from bell / OSC 9 / OSC 777 (`action_cb` → `notify.rs`, banners via native `UNUserNotificationCenter`, clicking one raises the window + selects the tab); three-state presence from the per-tab `probe` (cyan = exit 0, ghost = exit 3 restorable, hollow), doubling as end-session (☠ `kill` / ⏻ `suspend`, `main.rs::end_session`) or start (types `cmd`).
  - **Empty states**: warden is persistent (closing the last window doesn't quit; ⌘Q does). `manager.rs::sync_empty_surface` is the single authority for shell-core's home surface (`NoConfig` / `Broken` / `Windows`), reading the persisted `WindowManager::load_error` because it runs from places with no load context (a window's `Destroyed` handler). It re-installs warden's quit-when-last-surface-closes `Destroyed` handler on the home window the first time it's built. A mid-edit parse error with windows open shows an error banner instead.
  - **Menu**: App/Config/Window are shell-core's spine (`menu::build_spine`); the Tab submenu's navigation comes from `menu::build_tab_nav`/`tab_nav_action` (`tab_digit_keys`). warden's own: ⌘W unloads the tab (⌘⇧W closes the window), ⌘⇧O pops out, ⌘⇧S / ⌘⇧K suspend / terminate via chrome-core's `requestEnd`, and Reopen Last Closed (⌘⇧T) spliced into the Window submenu. Composition: `main.rs::build_app_menu`.
  - **Splits and pop-out**: a primary + optional secondary pane per tab (config `split` or ⌘D), and whole-tab, session-preserving pop-out (`GhosttySurface::reparent`, `Registry::detach`/`attach`). Mechanism and traps: [`docs/native-splits-direction.md`](docs/native-splits-direction.md).

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
reconcile.rs reconcile(old, new) → Reconciliation (open/close windows; per-window colour, tab add/remove/reorder, set_meta relabel, respawn_tabs)
load.rs      load(path); config_path() (WARDEN_CONFIG else ~/.config/warden/config.toml); LoadError
watch.rs     Watcher — parent-dir notify watcher; fires load() on change
bin/warden.rs  `warden validate` (warden's schema) + `warden fmt` (delegates to config-core's shared fmt_cli)
```

Formatting and colour live in config-core, re-exported at the crate root (`format_str`/`format_file`, `Colour`).

`crates/warden-app` — `plan.rs` (config → `WindowSpec`/`TabPlan`, reconcile → `WindowOp`; `derive_tree_meta`), `manager.rs` (`WindowManager`: materialize, apply reconciliations, `sync_empty_surface`, `effective_config` expanding roots), `scanner.rs`, `registry.rs` (per-window tab registry over surfaces), `surface/` + `ffi/` (the libghostty embed), `geometry.rs` (web-rect ↔ NSView-rect), `probe.rs`, `notify.rs`, `session.rs`. `ui/index.html` is the page shell + controller; the home surface is shell-core's `home.html`.

## Config schema (`~/.config/warden/config.toml`; override with `WARDEN_CONFIG`)

**Full reference in [`docs/config.md`](docs/config.md) — read it before touching parsing/resolution.** The README's *Every option* tables are the user-facing index of every key, so **a new key lands in `docs/config.md` and the README tables in the same change as the parser**. Load-bearing invariants:

- **`cmd` is typed *into* the shell, not exec'd** — it's libghostty `initial_input`, so a shell *function* like `amux` resolves and the shell survives the command exiting. Don't pass it as libghostty's `command`.
- **Tab identity is `id`-else-normalized-`dir`, never the title** (`Tab::key`); `title` is a repeatable display label. Curated and discovered tabs share the scheme, so a curated tab shadows a same-dir discovered project.
- **Groups add no cascade level; roots add one** (root → window → global). Both flatten to the single ordered `Tab` list with a `group` tag — the only downstream shape.
- **Resolution collapses the cascade** (`shell`/`cmd`/`probe`/`kill`/`suspend`/`split`, nearest level wins, `""` / `split = false` opts a level out); the app never sees levels. `density`/`tab_digit_keys` are global-only; `open_tabs_section` cascades global → window only, so it reaches live windows as an ordinary `WindowUpdate`.
- **Validation reports, never panics**; a missing `dir` is a warning (created anyway).

## Build / test / run

- `cargo build` / `cargo test`; `warden-app` compiles only on macOS, by design.
- **`just`** lists recipes: `just run` (against `examples/config.toml` — never your real config), `validate`, `test`, `fmt`, `clippy`, `gate` (the CI gate + config fmt-check + active-`[patch]` guard), `build`, `deploy`.
- **`just hooks` once per clone** — `core.hooksPath` is local config, so a fresh clone has neither the docgraph pre-push gate nor the active-`[patch]` pre-commit guard until it runs. CI (`.github/workflows/ci.yml`, macOS runner) is the backstop.
- **Toolchain is pinned** in `rust-toolchain.toml`; config-core carries the canonical pin — bump there first, then here and in curator and lector. The cores build with warden's toolchain, not their own.
- Vendored libghostty and revendoring: [`docs/surface.md`](docs/surface.md). Packaging, signing, releases, the updater: [`docs/releasing.md`](docs/releasing.md).

## Conventions & footguns

**The normal case is boring.** A TOML edit becomes windows + tabs; saving hot-reloads them through a straight config → materialize → reconcile pipeline. Almost everything below is a GUI-launch or native-embedding edge — real traps, but rare ones. Subsystem traps live in the docs in the reading-order table; this section holds the rest.

**Two premises the entries lean on:**

- **Minimal GUI PATH.** A `.app` opened from Dock/Finder/Spotlight inherits only `/usr/bin:/bin:/usr/sbin:/sbin`, so a bare-name shell/probe/kill command fails unless PATH is restored or the command is absolute.
- **A title change is destructive.** Changing a window's `title` is close(old) + open(new): its terminals and its persisted per-label bounds are recreated.

### Cross-cutting rules

- **No transitional fallbacks.** Finish every migration in-branch.
- **Keep tool-specific concepts out of `warden-config`** (neutral test strings). The README deliberately names [agentmux](https://github.com/lockyc/agentmux) as the companion — don't strip that, and don't leak it into the crate.
- **Raw-string TOML fixtures with a colour hex** self-terminate at `"#` inside `r#"…"#` — use `r##"…"##`.
- **NEVER move the user's real config aside to test `NoConfig`/`Broken`** — `WARDEN_CONFIG="$(mktemp -d)/nonexistent.toml"` reproduces `NoConfig` with nothing to restore. Moving it has destroyed a config once.
- **`open --env "WARDEN_CONFIG=<path>" -a /Applications/warden.app` is silently ignored if warden is already running** — and warden is persistent, so quit it first (`osascript -e 'quit app "warden"'`).
- **"No notifications at all" (not even the badge) is upstream transport, not warden.** The producer must wrap OSC 9/777 once per nested tmux layer (agentmux ≥ 0.14.0 does under `--frame`). Diagnose at the agent→frame boundary: `tmux -L agentmux-frame pipe-pane -t <agent-pane> 'cat >> /tmp/cap'` — a bare `]777` = dying at the frame; a `Ptmux;` envelope = it reaches warden.
- **Badge but no banner on macOS 26 is the OS, not `notify.rs`** — don't rewrite it. Discriminator and fixes: [`docs/notifications.md`](docs/notifications.md).
- **A link that won't open is the tmux hop or shift-⌘, not warden's link handling** — check order in [`docs/surface.md`](docs/surface.md).
- **A mousedown drags only if the element under the cursor carries `data-tauri-drag-region`** — children don't inherit it. chrome-core owns `#cc-titlebar`; warden's `#sidebar` carries the attribute.

### Call-site pointers (the named code comment carries the why)

- **Deploy launches via shell-core's `scripts/launch-app.sh`, never a bare `open`**, which forwards the deploying terminal's env (`TERM_PROGRAM`, `GHOSTTY_*`, `SHELL`, …) into every tab. Symptom: "misbehaves from `just deploy`, fine from Spotlight."
- **Default shell = `$SHELL -l`, absolute** (`main.rs::login_shell`); config shells may be bare, covered by **`restore_login_path`**, which imports the login-shell PATH at startup (best-effort, 3s deadline). Don't drop either expecting the other to cover it.
- **Probes inherit warden-app's env** — PATH is restored, shell exports aren't, so the canonical probe names `amux` absolutely (`"$HOME/.agentmux/bin/amux" --probe`). `probe.rs`.
- **The probe child's `$PWD` is the configured `dir`**, set explicitly in `probe.rs::run_probe` (also used by `end_session`) — a `$PWD`-keyed probe otherwise misses its session under a symlinked path.
- **The probe scheduler is the single driver** — don't serialize the sweep, drop the QoS demotion, batch the cache write, or add a one-shot reprobe. [`docs/probing.md`](docs/probing.md).
- **Never add an optimistic dot-clear on kill** (off→on→off flicker): the dot tracks `warden:session-state` alone; the command completes before a directional `bump_tab_await(false)`. `main.rs::end_session`.
- **`emit_to` leaks to sibling webviews** — every per-window event stamps a `label` and the chrome filters with `forMe()` (`main.rs` `on_menu_event`, `manager.rs` `warden:refresh`, `notify.rs`, `probe.rs`).
- **A new *global* setting won't hot-reload if only threaded through the per-window DTO** — a global-only edit is an empty reconcile, so add an old-vs-new compare in `main.rs`'s reload hook (launch works via `InitDto`, which hides it).
- **A hot-reload while the home surface lists windows must reconcile, not re-materialize** — key recovery on `last_good.windows.is_empty()`, not on zero windows open (`main.rs` reload hook).
- **Every load failure routes through `self.load_error` + `sync_empty_surface`** — never a direct `show_home`/`close_home` or a bespoke error window (`manager.rs`).
- **Format-on-save rewrites only on the clean-parse branch and only if bytes differ**, atomically; formatting stays idempotent so warden's own write settles after one retrigger (reload hook + config-core `format_file`).
- **The watcher matches the config by file name and fires on any event** (`warden-config/src/watch.rs`) — don't restore full-path equality or add an `event.kind` filter.
- **`rescan_root` diffs a fresh scan against `last_good`** (what's on screen), not two scans (`main.rs::rescan_root`).
- **Window bounds restore is the geometry plugin's own `on_window_ready` hook** — add no manual trigger (FOOTGUN in `manager.rs::build_window`).
- **A surface that fails to spawn degrades to a cold tab — never `.expect()`/`unwrap()`**; errors ride the init snapshot or `warden:error` (`registry.rs` `add`/`ensure_spawned`/`activate`).
- **Tab reconcile shapes:** a kept tab's `title`/`group`/`probe`/`kill`/`suspend`/`split.side`/`split.size` change is `set_meta` (no respawn); `dir`/`cmd`/`shell`/`load_on_open`/split presence/`split.cmd` rides `respawn_tabs`; `dir` with no `id` changes the key → remove + add (`reconcile.rs`).
- **Stale frontend embed:** `ui/` embeds at compile time; `build.rs` watches `ui/index.html` (add any new hand-written page, but don't watch `ui/` broadly — build.rs writes there). "My HTML change didn't take" → touch a `.rs`.
- **The empty-state placeholder is opaque and composited behind the content** (`#empty-state`); it lives in warden, not chrome-core.
- **Icon safe area** (824×824 on the 1024 canvas) is enforced by `assets/build-icons.sh` — design the SVG edge-to-edge.

## Shared cores: chrome-core and shell-core

warden, [curator](https://github.com/Lockyc/curator) (browser keeper-tabs) and [lector](https://github.com/Lockyc/lector) (local doc sites) share their sidebar and Tauri setup through git-rev-pinned cores. Change shared behaviour in the core, never here.

**[chrome-core](https://github.com/Lockyc/chrome-core) is the sidebar** — banner, tab rows and their dots, confirm row, density, resize, error bar, project tree, the Open section, and the self-updater. It's a build-dependency: `build.rs` writes `ui/chrome-core.{css,js}` (git-ignored) — edit its `assets/sidebar.{css,js}`, never the copies. `ui/index.html` is the page shell (`#terminal-hole`, `reportRect`, `forMe`, the first-probe handshake) plus a controller mapping the component's callbacks to warden commands and warden events to component calls.

- **A new `TabDto` field is invisible to the chrome until `toComponentDto` forwards it.** The Rust DTO and the component DTO are separate shapes hand-mapped in `ui/index.html`; add a field to both in the same change (this is how `tree`/`treePath` once rendered flat).
- warden-only DTO fields: `presence`, `killable`, `suspendable`, `startable`, `tree`/`treePath`; window-level `windowDrag` (`sidebar_drag`) and `openSection` (`open_tabs_section`). warden passes **no** `active`, so the component owns selection.
- **Dev loop:** `just chrome-dev` / `just chrome-pin` (likewise `config-*` and `shell-*`; scoped `#PATCH:<core>#` sentinels in the workspace `Cargo.toml`). **Never commit an active patch** — `just gate` and the pre-commit hook refuse. Visual tweaks: `just chrome-preview`.

**[shell-core](https://github.com/Lockyc/shell-core) is the byte-identical Tauri sliver** — release scripts (`materialize_scripts` → git-ignored `scripts/*.sh`, configured by the tracked `scripts/tooling.env`), the build stamp (`build_stamp()` → `BUILD_GIT_SHA`/`BUILD_DATE` in About), the menu spine and tab nav, the home surface, the detach window shell, and plugin registration incl. geometry persistence (`register_plugins(builder, Some(&config_path), &[])`). Its `build.rs` build-dep is `default-features = false` so it stays tauri-free; the normal dep enables `runtime`.

- **shell-core never touches config-core** — the home surface's "Create a starter config" is warden's own `shell_home_create_config`, calling `config_core::write_default_config` with `src/default-config.toml`.
- **`DetachSpec.panes` is a generic `Vec<f64>` of hole ratios**, not a warden split type — an empty vec keeps curator's and lector's payload unchanged.
- **Stays warden's own:** IPC fan-out (`emit_to` + `forMe()`), the config watcher (parses inside, drives root scans; curator/lector share `shell_core::watch`), and the warden-only Tab items. No chrome-caller command gate — warden's surfaces are native views with no webview to spoof a call.
- **`warden://open?window=<title>` is warden's own** (`Info.plist` `CFBundleURLTypes` → `RunEvent::Opened` → `main.rs::open_or_focus_window`, the one open-by-label path). If curator/lector want the same, lift the scheme handling into shell-core then.
