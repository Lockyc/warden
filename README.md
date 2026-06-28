<div align="center">

<img src="assets/icon-1024.png" alt="warden" width="128" height="128">

# warden

**A curator for your terminals** — profiles, projects, and (mostly) muxers all the way down.

</div>

warden is a **config-driven terminal multiplexer**. One TOML file is the source of truth: it defines **profiles** (windows) and the **project tabs** inside them. warden materializes itself from that config and **hot-reloads on save**. Each profile window carries a colour + name banner for at-a-glance identity; each tab is a real terminal opened in a working directory, running an optional command.

warden is **generic and content-agnostic** — it knows nothing about any specific tool, so the command a tab runs is whatever you want: a shell, a TUI, a build watcher, an agent launcher. It stands on its own.

It's also built for a flow: I pair each tab with [**agentmux**](https://github.com/lockyc/agentmux) (`amux`), a tmux-based agent launcher — so the full stack reads `libghostty → warden → agentmux → tmux`. A multiplexer for a multiplexer for a multiplexer; it's turtles the rest of the way down.

Targets **macOS**. Linux is a possible future direction, not a commitment; the config crate stays platform-neutral to keep that door open. Not Windows.

## Status

**Working on macOS.** Two layers are built:

- **Config-core** — the `warden-config` crate (parse / validate / resolve / diff / load / watch) plus a `warden validate` CLI.
- **The app** — `warden-app`, a macOS Tauri app embedding [libghostty](https://github.com/ghostty-org/ghostty) terminal surfaces. It opens one window per profile from the config (colour + name banner, curator-style draggable sidebar, terminal under an overlay titlebar), spawns project tabs (`keep_alive` eager at launch, the rest lazy on first focus), and **hot-reloads on save** (add/remove windows and tabs, recolour — live). A missing/invalid config opens a diagnostic window; a parse error on a live edit shows a banner and keeps the last-good windows up.

Deferred (see [`docs/FOLLOWUPS.md`](docs/FOLLOWUPS.md)): full keybindings + ad-hoc `cmd+T`/`cmd+N` tabs/windows, a controlled libghostty **source** build (the vendored binary is a throwaway prebuilt, currently blocked on a Zig 0.15.2 / macOS 26 SDK mismatch), and `TerminalSurface` seam + IPC hardening.

## Config

`~/.config/warden/config.toml` (override with `WARDEN_CONFIG`):

```toml
default_cmd = "fish -l"

[[profile]]                  # a window
name   = "work"
colour = "#0f8a8a"

  [[profile.tab]]            # a project terminal
  title      = "myproject"   # optional; defaults to the dir basename
  dir        = "~/code/myproject"
  cmd        = "tmux"        # optional; defaults to default_cmd
  keep_alive = true          # optional; spawn at launch and keep running
```

A profile is a window (its own colour + name banner); its tabs are project terminals. `keep_alive` tabs start at launch and keep running in the background.

## Build & use

```sh
cargo build
cargo test
cargo run -p warden-app                                # launch the app (macOS; reads WARDEN_CONFIG or ~/.config/warden/config.toml)
cargo run -p warden-config --bin warden -- validate    # validate ~/.config/warden/config.toml
cargo run -p warden-config --bin warden -- validate path/to/config.toml
```

`warden-app` materializes one window per profile and hot-reloads on save; edit the config while it's running to watch windows and tabs appear, disappear, and recolour live.

`warden validate` prints the resolved profiles/tabs and any warnings; exit code 0 (ok), 1 (load/parse/validation error), 2 (usage).

## Layout

- `crates/warden-config/` — the config crate (library + `warden` CLI).
- `crates/warden-app/` — the macOS Tauri app: windows, tab strip, libghostty surfaces behind the `TerminalSurface` seam, and hot-reload wiring.
- `assets/` — icon masters (`icon.svg`, `icon-app.svg`), rendered PNGs, the macOS `warden.icns`, and `build-icons.sh` to regenerate the rasters from the SVGs.
- `docs/FOLLOWUPS.md` — tracked list of intentionally-deferred work.

## License

TBD.
