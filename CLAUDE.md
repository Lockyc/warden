# warden — agent orientation

## What warden is

warden is a **config-driven terminal multiplexer** — "curator for terminals." A single TOML file is the source of truth: it defines **profiles** (windows) and the **project tabs** inside them. The app materializes itself from that config and **hot-reloads on save**. Each profile window carries a colour + name banner for at-a-glance identity; each tab is a real terminal opened in a working directory running an optional command. warden is **generic and content-agnostic** — it knows nothing about any specific tool; the command a tab runs is arbitrary (a shell, a TUI, an agent launcher, whatever).

Target platform: **macOS**. Linux is a *possible future consideration, not a guarantee* — the `warden-config` crate is kept platform-neutral (pure logic, no macOS-only APIs) so that door stays open, but nothing commits to shipping Linux. Not Windows.

## Current state — read this before assuming anything exists

Two layers are built:

- **Config-core** — the `warden-config` crate (pure logic, no GUI): parses/validates/resolves the TOML, diffs two configs for hot-reload, loads from disk, watches the file, ships a `warden validate` CLI.
- **GUI** — the `warden-app` crate: a config-driven, multi-window **macOS-only** Tauri app. It loads the config at launch and materializes **one native window per profile**, each with its colour + name banner; tabs are embedded libghostty terminal surfaces behind the `TerminalSurface` seam; each tab lazy-spawns on first focus, except `keep_alive = true` tabs which spawn at window open and keep running for background work. Chrome is curator-style: a transparent window with an Overlay titlebar (terminal reaches the top, traffic lights over the sidebar), an opaque draggable sidebar (banner + tab list). Each tab row carries a **live/cold dot** (filled green = surface spawned; hollow ring = cold) that doubles as an **unload** control: hovering a live dot reveals a red ✕ that kills that terminal (surface + PTY) — the tab goes cold and respawns a fresh shell on next focus (the normal lazy-spawn path, *not* a curator-style cheap reload). Unloading the visible tab switches to an already-**live** neighbour (next if live, else the nearest live tab, leaning to the previous side) — keyboard navigation never wakes a cold tab, so if nothing else is live the hole blanks until you pick a tab. Tabs also surface **terminal notifications**: when a tab rings the bell or emits an OSC 9 / OSC 777 desktop-notification escape (decoded from libghostty's `action_cb` → a seam-neutral event → `notify.rs`), warden badges that tab's row with an amber dot when it isn't the visible tab, and a desktop-notification additionally raises a macOS banner; the badge clears on focus. Any program printing the standard escape benefits — this is the channel agentmux's Claude hooks feed (tmux passthrough + OSC 777) instead of shelling `osascript`. **Hot-reload** is live (watcher + `reconcile` → per-window open/close/refresh; last-window-quit). A missing/invalid/empty config opens a **diagnostic window**; a parse error on a live save shows an **error banner** in the chrome and keeps last-good windows up; a recovered config materializes and closes the diagnostic.

Tab navigation is wired through the app menu: **⌘⇧[ / ⌘⇧]** cycle the previous/next tab but **only among loaded tabs** (cold tabs are skipped — cycling never spawns), **⌘1–⌘9** jump to a position, **⌘W** unloads the active tab, **⌘⇧W** closes the window, plus **⌘Q/⌘M**. (Safari/Chrome convention: ⌘W = close *tab*, ⌘⇧W = close *window*; here close-tab means unload.) Still deferred (see `docs/FOLLOWUPS.md`): `cmd+\`` profile-window cycling and ad-hoc tabs (`cmd+T`/`cmd+N`); a real **libghostty source build** (the vendored `libghostty.a` is a throwaway prebuilt; the source build is blocked on Zig 0.15.2 vs the macOS 26 SDK); `TerminalSurface` seam hardening (object-safety, `Drop`-based teardown, platform-opaque window handle, `cfg(macos)` gating); FFI repr hardening; libghostty lifecycle behind the seam; a real CSP + IPC hardening; watcher debounce; per-profile window icon; argv passthrough decision.

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
shell = "fish -l"                      # optional; global default shell every tab spawns (default "fish -l")
cmd   = "amux"                         # optional; global default startup command run inside the shell

[[profile]]                            # = a window
name   = "work"                        # required, unique, non-empty; banner + window title
colour = "#0f8a8a"                     # required; #rgb or #rrggbb
icon   = "~/…/work.png"                # optional; window proxy icon (macOS)
shell  = "zsh"                         # optional; per-profile shell override
cmd    = "amux"                        # optional; per-profile startup override

  [[profile.tab]]                      # = a project terminal
  title      = "locus"                 # optional; default = basename(dir); must be non-empty & unique within the profile
  dir        = "~/Developer/…/locus"   # required
  shell      = "bash"                  # optional; per-tab shell override
  cmd        = "amux"                  # optional; per-tab startup override ("" = opt out → bare shell)
  keep_alive = true                    # optional; default false (spawn at launch + keep running for background work)
```

**`shell` and `cmd` cascade global → profile → tab; the nearest set level wins.** A missing value at a level inherits from above; `shell` falls back to the built-in `"fish -l"` if unset everywhere; `cmd` is `None` (bare shell) if unset everywhere. An explicitly-empty `cmd = ""` counts as *set* and resets to `None`, so it opts a level out of an inherited command instead of inheriting it (see `cascade()` in `resolve.rs`). `shell` is treated the same (empty = unset). Resolution collapses the cascade into the flat `Tab.shell: String` + `Tab.startup: Option<String>` — the app never sees the levels.

**`cmd` runs inside the shell, it doesn't replace it.** Every tab spawns its resolved `shell` (an interactive shell under the PTY); the resolved `cmd`, if any, is delivered as libghostty `initial_input` — i.e. *typed into* that shell (newline-terminated) rather than exec'd directly. This is deliberate and load-bearing: `amux` is a shell **function**, not an executable, so execing it directly fails — only an interactive shell resolves it. As a bonus the shell stays live after the command exits (detaching from `amux`/tmux drops you to a prompt, not a dead pane). Do **not** "fix" this back to passing `cmd` as libghostty's `command`/exec target.

Validation: unique profile name, unique tab title within a profile, non-empty name/dir/explicit-title, valid colour → **errors**; a `dir` that doesn't exist → **warning** (tab still created). Invalid config must be reported, never panic.

## Build / test / run

- `cargo build` / `cargo test` (workspace). `warden-app` is macOS-only (libghostty embed); it fails to compile elsewhere by design.
- `cargo run -p warden-config --bin warden -- validate [path]` — validates a config and prints the resolved profile/tab tree + warnings (exit 0 ok / 1 load error / 2 usage).
- `cargo run -p warden-app` — launch the GUI (reads `WARDEN_CONFIG` or `~/.config/warden/config.toml`).
- **`justfile`** wraps the common flows: `just run` (launches the app against `examples/config.toml` via `WARDEN_CONFIG`, so iterating never touches your real config), `just validate [path]`, `just test`, `just check`, `just fmt`, `just clippy`; bare `just` lists them.
- **Packaging:** `just build` produces the release `warden.app` (needs the Tauri CLI — `cargo install tauri-cli --version ^2`); `just deploy` installs it to `/Applications` and relaunches. The build is **not notarized** (Tauri code-signs it with your local Apple Development identity if one is in the keychain, else leaves it unsigned); `scripts/install-app.sh` strips the Gatekeeper quarantine xattr so the local copy runs either way. Distributing to *other* machines would need Developer ID signing + notarization (deferred, see `docs/FOLLOWUPS.md`). The bundle icon is `crates/warden-app/icons/icon.icns`, produced from the SVG masters by `assets/build-icons.sh`.

## Conventions & footguns (things that have bitten us / will bite again)

- **macOS app-icon safe area (the "our icon is bigger than everyone else's" bug):** Apple's icon grid sits the rounded tile in an **824×824 box centred on the 1024 canvas (~100px transparent margin each side)**. An edge-to-edge tile renders ~25% oversized in the Dock / cmd-Tab switcher next to every other app. `assets/icon-app.svg` deliberately draws the tile **edge-to-edge** (design it full-bleed); the margin is enforced in `assets/build-icons.sh`, which embeds the tile as a data: URI into an 824/1024 wrapper before rasterising — so the margin can't silently regress and the art never has to encode it. Do **not** "fix" this by making the SVG full-canvas again, and do **not** drop the wrapper. (This class of bug has recurred on every app; the script-level enforcement is the durable fix.)

- **Raw-string test fixtures with a colour hex:** a Rust `r#"..."#` literal containing `colour = "#..."` self-terminates early at the `"#`. Use `r##"..."##` for any TOML test fixture that includes a hex colour. (Hit repeatedly across modules.)
- **The watcher matches the config file by file name, not full path** (`watch.rs`) — deliberately. This survives macOS FSEvents reporting `/private/var/...` vs a caller's `/var/...` symlink, and survives atomic-save editors that swap the file inode (the watch is on the parent dir). Do **not** "fix" it back to full-path equality, and do **not** add an `event.kind` filter — spec requires firing on *any* event for the file (atomic-save renames can surface as `Create`).
- **Tab identity is the resolved title** (`Tab::key`). `reconcile` diffs profiles by `name` and tabs by this key. In-place edits to a kept tab's `dir`/`cmd`/`keep_alive` (title unchanged) are **not** detected — the consumer reopens the tab to apply them. A profile **rename** is destructive: it appears as close(old)+open(new), killing and recreating that window's terminals (including `keep_alive` ones).
- **Generic:** keep tool-specific concepts out of the crate. Example command strings in tests stay neutral. The line between crate and public face: the **crate stays neutral**, but the **README/public face deliberately references [agentmux](https://github.com/lockyc/agentmux) (`amux`)** — a tmux-based agent launcher — as the canonical companion, since pairing warden tabs with it is the author's intended flow (the `libghostty → warden → agentmux → tmux` stack). Do not "degenericise" the README by stripping that link, and do not leak agentmux into the crate to match it.
- **warden scrubs `$TMUX`/`$TMUX_PANE` from spawned terminals' env** (`main.rs` `scrub_inherited_tmux_env`, called first thing in `main()`). warden is routinely launched from inside the very agentmux/tmux session it exists to host, and libghostty hands each surface's shell warden-app's own environment — so a leaked `$TMUX` makes nested tmux/agentmux think they're inside another tmux and refuse to open frames / misroute prefix keys. Don't remove the scrub, and if env passthrough is ever added to the surface spawn, keep these vars stripped (extend the list if other launcher-context vars cause the same nesting confusion).
- **App-menu shortcuts vs libghostty keybinds (the "tab hotkey only sometimes works" bug):** macOS sends a ⌘-combo to the key window's view hierarchy `performKeyEquivalent:` **before** the main menu, and libghostty has its *own* built-in keybinds for the standard tab chords (`⌘⇧[`/`⌘⇧]` = prev/next tab, `⌘1–9` = goto-tab — the exact chords warden's Tab menu uses). So if `WardenHostView::performKeyEquivalent:` forwards a chord to libghostty first, libghostty matches its keybind, returns *consumed = true*, the view returns `YES`, and the event is swallowed before warden's menu item ever fires. The fix (in `surface/ghostty.rs`): give the **main menu first refusal** — `NSApplication::sharedApplication().mainMenu().performKeyEquivalent(event)` at the top of `performKeyEquivalent:`; if it returns true the menu owns the chord, so stop. Only forward to libghostty when the menu declines. The menu's own accelerators define the reserved set, so it self-maintains — but don't "simplify" by forwarding to libghostty first, and remember any new menu accelerator automatically wins over a colliding ghostty keybind (that's intended).
- **Keep the migration complete:** this is a young codebase; finish changes in-branch, no transitional fallbacks.
- **Stale frontend embed:** `tauri-build` (2.6.x) emits no `rerun-if-changed` for `frontendDist`, and the `ui/` assets are embedded at compile time via `generate_context!`. So after a *frontend-only* edit (`index.html`/`diagnostic.html`), `cargo run` silently serves the **stale** HTML until some Rust change forces a recompile. `crates/warden-app/build.rs` emits explicit `cargo:rerun-if-changed=ui/...` lines to defeat this — keep them. If you edit the frontend and the change "doesn't take," suspect a stale embed (touch a `.rs` file or `cargo clean -p warden-app`).

## Relationship to curator (the chrome reference)

warden's window chrome (the sidebar: per-window colour banner, vertical tab list, overlay titlebar, hot-reload error banner) is the same *silhouette* as **curator** ([github.com/Lockyc/curator](https://github.com/Lockyc/curator)) — a sibling macOS Tauri app that curates **browser keeper-tabs** (webviews), where warden curates **terminals** (libghostty surfaces). Both are **public** GitHub repos under `Lockyc` (warden is set private only until it's release-ready). So any shared chrome layer must keep *both* self-contained — a public shared module is viable; a private one would break the other's clones. curator's chrome is the older, richer one (grouped tabs with sticky section headers, a per-tab loaded/unload dot, unread badges, a nav pill). warden's chrome was re-derived from a *description* of curator rather than ported from its files, and silently lost some of that richness (the vertical tab-row treatment had to be re-added by hand). To stop that recurring:

- **curator is the canonical chrome reference — port from it, don't re-derive.** Before building or restyling chrome, diff against curator's `src/chrome.css` / `src/chrome.js` and lift what applies, rather than reinventing it. This is the footgun that already bit us once.
- **Do NOT share a crate or component between the two.** It's tempting and it's wrong here. The config schemas genuinely diverge (curator: `[[window]]/[[group]]/[[tab]]` with URLs + login sessions; warden: `[[profile]]/[[tab]]` with `dir`/`cmd`/`keep_alive`), the Rust cores diverge (curator: webview z-order / escape-click / notification injection; warden: libghostty FFI + a native surface hole), and the two live in separate repos with different build systems (curator npm + `src/`; warden cargo + `ui/` embedded via `generate_context!`). A shared crate would be a forced abstraction over two real designs. **If** sharing is ever justified (a third app appears, or the chrome grows large), the unit is a small **web design layer — CSS + a vanilla sidebar component — never a Rust/config crate**, because the overlap is the *look* and the divergence is the *data + native plumbing*.
- **Not every curator chrome feature ports.** Browser-only affordances (favicons-from-URL, back/forward/home/reload) don't map to terminals — in particular warden deliberately omits curator's **click-the-active-tab-to-reload** (`home_tab`) and its cheap webview reload: a terminal has session state, so there is nothing to cheaply re-fetch. Three curator features are now **shipped, re-interpreted for terminals**: the **per-tab letter/colour tile** (title initial + hashed colour — pure presentation, no new data); the **live/cold dot + unload control** — the dot shows whether a surface is spawned (free from `registry.rs::is_spawned`), and its hover-✕ *kills* the terminal (surface + PTY); the tab then respawns a fresh shell on next focus via the normal lazy path (never curator's reload-on-click); and **unread badges**, which once "needed a producer" but now have one — warden decodes libghostty's `RING_BELL` / `DESKTOP_NOTIFICATION` (OSC 9/777) actions and badges the owning tab (amber dot, + macOS banner for notifications) when it isn't visible. One feature *looks* portable but stays deferred on a prerequisite: **grouped tabs** need a config-schema decision (warden tabs are flat — no `group`).

## Deferred work

Tracked in `docs/FOLLOWUPS.md` (e.g. crate-root re-exports and `serde::Serialize` for the future Tauri IPC, watcher debounce, edge cases). Consult it before re-deriving a "missing" feature — it may be a conscious deferral.
