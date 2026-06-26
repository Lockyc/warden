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

**Early / work in progress.** Today the repo contains the **config-core** layer only — the `warden-config` Rust crate that parses, validates, resolves, diffs, loads, and watches the config, plus a `warden validate` CLI. The GUI (a Tauri app embedding [libghostty](https://github.com/ghostty-org/ghostty) terminal surfaces, with per-profile windows and tabbed projects) is not built yet.

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
cargo run -p warden-config --bin warden -- validate   # validate ~/.config/warden/config.toml
cargo run -p warden-config --bin warden -- validate path/to/config.toml
```

`warden validate` prints the resolved profiles/tabs and any warnings; exit code 0 (ok), 1 (load/parse/validation error), 2 (usage).

## Layout

- `crates/warden-config/` — the config crate (library + `warden` CLI).
- `assets/` — icon masters (`icon.svg`, `icon-app.svg`), rendered PNGs, the macOS `warden.icns`, and `build-icons.sh` to regenerate the rasters from the SVGs.
- `docs/FOLLOWUPS.md` — tracked list of intentionally-deferred work.

## License

TBD.
