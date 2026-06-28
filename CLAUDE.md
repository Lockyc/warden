# warden — agent orientation

## What warden is

warden is a **config-driven terminal multiplexer** — "curator for terminals." A single TOML file is the source of truth: it defines **profiles** (windows) and the **project tabs** inside them. The app materializes itself from that config and **hot-reloads on save**. Each profile window carries a colour + name banner for at-a-glance identity; each tab is a real terminal opened in a working directory running an optional command. warden is **generic and content-agnostic** — it knows nothing about any specific tool; the command a tab runs is arbitrary (a shell, a TUI, an agent launcher, whatever).

Target platform: **macOS**. Linux is a *possible future consideration, not a guarantee* — the `warden-config` crate is kept platform-neutral (pure logic, no macOS-only APIs) so that door stays open, but nothing commits to shipping Linux. Not Windows.

## Current state — read this before assuming anything exists

Two layers are built:

- **Config-core** — the `warden-config` crate (pure logic, no GUI): parses/validates/resolves the TOML, diffs two configs for hot-reload, loads from disk, watches the file, ships a `warden validate` CLI.
- **GUI** — the `warden-app` crate: a config-driven, multi-window **macOS-only** Tauri app. It loads the config at launch and materializes **one native window per profile**, each with its colour + name banner; tabs are embedded libghostty terminal surfaces behind the `TerminalSurface` seam; each tab lazy-spawns on first focus, except `keep_alive = true` tabs which spawn at window open and keep running for background work. Chrome is curator-style: a transparent window with an Overlay titlebar (terminal reaches the top, traffic lights over the sidebar), an opaque draggable sidebar (banner + tab list). **Hot-reload** is live (watcher + `reconcile` → per-window open/close/refresh; last-window-quit). A missing/invalid/empty config opens a **diagnostic window**; a parse error on a live save shows an **error banner** in the chrome and keeps last-good windows up; a recovered config materializes and closes the diagnostic.

Still deferred (see `docs/FOLLOWUPS.md`): full keybindings (`cmd+\``/tab-cycle) and ad-hoc tabs (`cmd+T`/`cmd+N`); a real **libghostty source build** (the vendored `libghostty.a` is a throwaway prebuilt; the source build is blocked on Zig 0.15.2 vs the macOS 26 SDK); `TerminalSurface` seam hardening (object-safety, `Drop`-based teardown, platform-opaque window handle, `cfg(macos)` gating); FFI repr hardening; libghostty lifecycle behind the seam; a real CSP + IPC hardening; watcher debounce; per-profile window icon; argv passthrough decision.

## Intended architecture (where it's going)

- **Tauri** (Rust core + web chrome), one app, macOS-first. Tauri's cross-platform nature keeps a future Linux port viable, but macOS is the only committed target.
- **Profiles = separate native windows** (macOS `cmd+\`` cycles them), each with its own colour + name banner. **Tabs = projects** within a window, cycled by a secondary keybinding. A single app/dock-icon by design — per-window identity is carried in-app via the banner, not via separate app bundles.
- **Terminal surface = embedded libghostty** behind a `TerminalSurface` seam — a small per-OS native shim hosts the surface (NSView on macOS; a GTK widget would host it on Linux if that port ever happens). A non-libghostty fallback impl (e.g. SwiftTerm-class) can sit behind the same seam. libghostty's embedding C API is officially unstable — **pin it** to a known commit.
- The `warden-config` crate is the foundation the app consumes (window/tab set, hot-reload reconcile, watcher).

## Workspace layout

Cargo workspace. `crates/warden-config` — the config crate (a library plus the `warden` CLI binary). Data flows one direction through small, independently-tested modules:

```
raw.rs       serde structs mirroring the TOML schema + parse()
  ↓
resolve.rs   validate + fill defaults + expand ~ → (Config, Vec<Warning>); ResolveError
  ↓
model.rs     clean resolved types: Config / Profile / Tab / Warning
reconcile.rs reconcile(old, new) → Reconciliation (open/close profiles, per-profile colour/icon/tab-add-remove/tab-reorder) for hot-reload
load.rs      load(path) = read+parse+resolve; config_path() (WARDEN_CONFIG override else ~/.config/warden/config.toml); LoadError
watch.rs     Watcher — notify-based parent-dir file watcher; fires load() on change
bin/warden.rs  `warden validate [path]` CLI
```

`crates/warden-app` — the macOS Tauri app that consumes the config crate. Key modules: `plan.rs` (config → `WindowSpec`/`TabPlan`, `reconcile` → `WindowOp`), `manager.rs` (`WindowManager`: materialize windows, apply reconciliations, diagnostic window), `registry.rs` (per-window tab registry over surfaces), `surface/` + `ffi/` (the libghostty embed behind the `TerminalSurface` seam — macOS/objc2), `geometry.rs` (web-rect ↔ NSView-rect). `ui/index.html` is the chrome; `ui/diagnostic.html` the config-error page.

## Config schema (`~/.config/warden/config.toml`; override with `WARDEN_CONFIG`)

```toml
default_cmd = "fish -l"               # optional; tabs with no cmd use this

[[profile]]                            # = a window
name   = "work"                        # required, unique, non-empty; banner + window title
colour = "#0f8a8a"                     # required; #rgb or #rrggbb
icon   = "~/…/work.png"                # optional; window proxy icon (macOS)

  [[profile.tab]]                      # = a project terminal
  title      = "locus"                 # optional; default = basename(dir); must be non-empty & unique within the profile
  dir        = "~/Developer/…/locus"   # required
  cmd        = "amux"                  # optional; default = default_cmd → "fish -l"
  keep_alive = true                    # optional; default false (spawn at launch + keep running for background work)
```

Validation: unique profile name, unique tab title within a profile, non-empty name/dir/explicit-title, valid colour → **errors**; a `dir` that doesn't exist → **warning** (tab still created). Invalid config must be reported, never panic.

## Build / test / run

- `cargo build` / `cargo test` (workspace). `warden-app` is macOS-only (libghostty embed); it fails to compile elsewhere by design.
- `cargo run -p warden-config --bin warden -- validate [path]` — validates a config and prints the resolved profile/tab tree + warnings (exit 0 ok / 1 load error / 2 usage).
- `cargo run -p warden-app` — launch the GUI (reads `WARDEN_CONFIG` or `~/.config/warden/config.toml`).
- **`justfile`** wraps the common flows: `just run` (launches the app against `examples/config.toml` via `WARDEN_CONFIG`, so iterating never touches your real config), `just validate [path]`, `just test`, `just check`, `just fmt`, `just clippy`; bare `just` lists them. No `build`/`deploy` recipes yet — packaging is deferred (see `docs/FOLLOWUPS.md`).

## Conventions & footguns (things that have bitten us / will bite again)

- **Raw-string test fixtures with a colour hex:** a Rust `r#"..."#` literal containing `colour = "#..."` self-terminates early at the `"#`. Use `r##"..."##` for any TOML test fixture that includes a hex colour. (Hit repeatedly across modules.)
- **The watcher matches the config file by file name, not full path** (`watch.rs`) — deliberately. This survives macOS FSEvents reporting `/private/var/...` vs a caller's `/var/...` symlink, and survives atomic-save editors that swap the file inode (the watch is on the parent dir). Do **not** "fix" it back to full-path equality, and do **not** add an `event.kind` filter — spec requires firing on *any* event for the file (atomic-save renames can surface as `Create`).
- **Tab identity is the resolved title** (`Tab::key`). `reconcile` diffs profiles by `name` and tabs by this key. In-place edits to a kept tab's `dir`/`cmd`/`keep_alive` (title unchanged) are **not** detected — the consumer reopens the tab to apply them. A profile **rename** is destructive: it appears as close(old)+open(new), killing and recreating that window's terminals (including `keep_alive` ones).
- **Generic:** keep tool-specific concepts out of the crate. Example command strings in tests stay neutral. The line between crate and public face: the **crate stays neutral**, but the **README/public face deliberately references [agentmux](https://github.com/lockyc/agentmux) (`amux`)** — a tmux-based agent launcher — as the canonical companion, since pairing warden tabs with it is the author's intended flow (the `libghostty → warden → agentmux → tmux` stack). Do not "degenericise" the README by stripping that link, and do not leak agentmux into the crate to match it.
- **Keep the migration complete:** this is a young codebase; finish changes in-branch, no transitional fallbacks.
- **Stale frontend embed:** `tauri-build` (2.6.x) emits no `rerun-if-changed` for `frontendDist`, and the `ui/` assets are embedded at compile time via `generate_context!`. So after a *frontend-only* edit (`index.html`/`diagnostic.html`), `cargo run` silently serves the **stale** HTML until some Rust change forces a recompile. `crates/warden-app/build.rs` emits explicit `cargo:rerun-if-changed=ui/...` lines to defeat this — keep them. If you edit the frontend and the change "doesn't take," suspect a stale embed (touch a `.rs` file or `cargo clean -p warden-app`).

## Relationship to curator (the chrome reference)

warden's window chrome (the sidebar: per-window colour banner, vertical tab list, overlay titlebar, hot-reload error banner) is the same *silhouette* as **curator** ([github.com/Lockyc/curator](https://github.com/Lockyc/curator)) — a sibling macOS Tauri app that curates **browser keeper-tabs** (webviews), where warden curates **terminals** (libghostty surfaces). curator's chrome is the older, richer one (grouped tabs with sticky section headers, a per-tab loaded/unload dot, unread badges, a nav pill). warden's chrome was re-derived from a *description* of curator rather than ported from its files, and silently lost some of that richness (the vertical tab-row treatment had to be re-added by hand). To stop that recurring:

- **curator is the canonical chrome reference — port from it, don't re-derive.** Before building or restyling chrome, diff against curator's `src/chrome.css` / `src/chrome.js` and lift what applies, rather than reinventing it. This is the footgun that already bit us once.
- **Do NOT share a crate or component between the two.** It's tempting and it's wrong here. The config schemas genuinely diverge (curator: `[[window]]/[[group]]/[[tab]]` with URLs + login sessions; warden: `[[profile]]/[[tab]]` with `dir`/`cmd`/`keep_alive`), the Rust cores diverge (curator: webview z-order / escape-click / notification injection; warden: libghostty FFI + a native surface hole), and the two live in separate repos with different build systems (curator npm + `src/`; warden cargo + `ui/` embedded via `generate_context!`). A shared crate would be a forced abstraction over two real designs. **If** sharing is ever justified (a third app appears, or the chrome grows large), the unit is a small **web design layer — CSS + a vanilla sidebar component — never a Rust/config crate**, because the overlap is the *look* and the divergence is the *data + native plumbing*.
- **Not every curator chrome feature ports.** Browser-only affordances (favicons-from-URL, back/forward/home/reload, reload-to-unload) don't map to terminals. Of the three that look portable, each carries a prerequisite, so don't port the UI blind: **grouped tabs** need a config-schema decision (warden tabs are flat — no `group`); **unread badges** need a terminal-activity *producer* (nothing emits them — would mean hooking libghostty's bell/output); and the **loaded/unload dot** depends on an unload action whose "free memory, reload on click" semantics are wrong for a terminal (it would kill the live session, not cheaply reload it). The genuinely free win is presentational only (e.g. a per-tab letter/colour tile derived from the title — no new data, no destructive action).

## Deferred work

Tracked in `docs/FOLLOWUPS.md` (e.g. crate-root re-exports and `serde::Serialize` for the future Tauri IPC, watcher debounce, edge cases). Consult it before re-deriving a "missing" feature — it may be a conscious deferral.
