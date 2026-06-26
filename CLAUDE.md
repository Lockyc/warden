# warden — agent orientation

## What warden is

warden is a **config-driven terminal multiplexer** — "curator for terminals." A single TOML file is the source of truth: it defines **profiles** (windows) and the **project tabs** inside them. The app materializes itself from that config and **hot-reloads on save**. Each profile window carries a colour + name banner for at-a-glance identity; each tab is a real terminal opened in a working directory running an optional command. warden is **generic and content-agnostic** — it knows nothing about any specific tool; the command a tab runs is arbitrary (a shell, a TUI, an agent launcher, whatever).

Target platform: **macOS**. Linux is a *possible future consideration, not a guarantee* — the `warden-config` crate is kept platform-neutral (pure logic, no macOS-only APIs) so that door stays open, but nothing commits to shipping Linux. Not Windows.

## Current state — read this before assuming anything exists

Only the **config-core layer** is built: the `warden-config` crate (pure logic, no GUI). It parses/validates/resolves the TOML, diffs two configs for hot-reload, loads from disk, watches the file, and ships a `warden validate` CLI.

**The GUI does not exist yet** — no Tauri app, no windows, no libghostty terminal surfaces, no banners/tab-strip. Don't assume an app, a window, or a rendered terminal is present. That's future work (see "Intended architecture").

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

- `cargo build` / `cargo test` (workspace).
- `cargo run -p warden-config --bin warden -- validate [path]` — validates a config and prints the resolved profile/tab tree + warnings (exit 0 ok / 1 load error / 2 usage).
- No `justfile` yet — add one when the GUI app lands.

## Conventions & footguns (things that have bitten us / will bite again)

- **Raw-string test fixtures with a colour hex:** a Rust `r#"..."#` literal containing `colour = "#..."` self-terminates early at the `"#`. Use `r##"..."##` for any TOML test fixture that includes a hex colour. (Hit repeatedly across modules.)
- **The watcher matches the config file by file name, not full path** (`watch.rs`) — deliberately. This survives macOS FSEvents reporting `/private/var/...` vs a caller's `/var/...` symlink, and survives atomic-save editors that swap the file inode (the watch is on the parent dir). Do **not** "fix" it back to full-path equality, and do **not** add an `event.kind` filter — spec requires firing on *any* event for the file (atomic-save renames can surface as `Create`).
- **Tab identity is the resolved title** (`Tab::key`). `reconcile` diffs profiles by `name` and tabs by this key. In-place edits to a kept tab's `dir`/`cmd`/`keep_alive` (title unchanged) are **not** detected — the consumer reopens the tab to apply them. A profile **rename** is destructive: it appears as close(old)+open(new), killing and recreating that window's terminals (including `keep_alive` ones).
- **Generic:** keep tool-specific concepts out of the crate. Example command strings in tests stay neutral. The line between crate and public face: the **crate stays neutral**, but the **README/public face deliberately references [agentmux](https://github.com/lockyc/agentmux) (`amux`)** — a tmux-based agent launcher — as the canonical companion, since pairing warden tabs with it is the author's intended flow (the `libghostty → warden → agentmux → tmux` stack). Do not "degenericise" the README by stripping that link, and do not leak agentmux into the crate to match it.
- **Keep the migration complete:** this is a young codebase; finish changes in-branch, no transitional fallbacks.

## Deferred work

Tracked in `docs/FOLLOWUPS.md` (e.g. crate-root re-exports and `serde::Serialize` for the future Tauri IPC, watcher debounce, edge cases). Consult it before re-deriving a "missing" feature — it may be a conscious deferral.
