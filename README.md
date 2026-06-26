# warden

A **config-driven, cross-platform terminal multiplexer** — think "curator for terminals." One TOML file defines **profiles** (windows) and the **project tabs** inside them; warden materializes itself from that config and hot-reloads on save. Each profile window has a colour + name banner; each tab is a real terminal opened in a project directory running an optional command. warden is generic — the command a tab runs is up to you (a shell, a TUI, an agent launcher, anything).

Targets macOS (primary) and Linux.

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
- `docs/FOLLOWUPS.md` — tracked list of intentionally-deferred work.

## License

TBD.
