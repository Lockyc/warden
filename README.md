<div align="center">

<img src="assets/icon-1024.png" alt="warden" width="128" height="128">

# warden

**A curator for your terminals** — windows, projects, and (mostly) muxers all the way down.

</div>

warden is a **config-driven terminal multiplexer**. One TOML file is the source of truth: it defines **windows** and the **project tabs** inside them. warden materializes itself from that config and **hot-reloads on save**. Each window carries a colour + name banner for at-a-glance identity; each tab is a real terminal opened in a working directory, running an optional command.

warden is **generic and content-agnostic** — it knows nothing about any specific tool, so the command a tab runs is whatever you want: a shell, a TUI, a build watcher, an agent launcher. It stands on its own.

It's also built for a flow: I pair each tab with [**agentmux**](https://github.com/lockyc/agentmux) (`amux`), a tmux-based agent launcher — so the stack nests `warden → agentmux → tmux` (warden itself embedding [libghostty](https://github.com/ghostty-org/ghostty) as its terminal surfaces). A multiplexer for a multiplexer for a multiplexer; it's turtles the rest of the way down.

Targets **macOS**. Linux is a possible future direction, not a commitment; the config crate stays platform-neutral to keep that door open. Not Windows.

## Status

**Working on macOS.** Public project, currently a private repo until it's ready for release. Two layers are built:

- **Config-core** — the `warden-config` crate (parse / validate / resolve / diff / load / watch) plus a `warden validate` CLI.
- **The app** — `warden-app`, a macOS Tauri app embedding [libghostty](https://github.com/ghostty-org/ghostty) terminal surfaces. It opens a window for each `[[window]]` in the config (colour + name banner, curator-style draggable sidebar, terminal under an overlay titlebar), spawns project tabs (`keep_alive` eager at launch, the rest lazy on first focus), and **hot-reloads on save** (add/remove windows and tabs, recolour, re-section groups — live). A missing/invalid config opens a diagnostic window; a parse error on a live edit shows a banner and keeps the last-good windows up. Switch tabs from the **Tab** menu — **⌘⇧[** / **⌘⇧]** cycle the previous/next *loaded* tab (cold tabs are skipped) and **⌘1–⌘9** jump to a position; set `tab_digit_keys = "cycle"` to instead make **⌘1** / **⌘2** cycle next/prev (jumps then shift to **⌘3–⌘9**). **⌘W** unloads the active tab and **⌘⇧W** closes the window (Safari/Chrome convention).
- **Tab-row affordances** — each sidebar tab shows a letter/colour tile and a **live/cold dot**: filled when the terminal is spawned, hollow when cold. Hovering a live dot reveals a ✕ that **unloads** the tab — kills the terminal and PTY; it goes cold and respawns a fresh shell on next focus. Tabs also **surface notifications**: when a background tab rings the bell or emits a desktop-notification escape (OSC 9 / OSC 777), warden badges its row with an amber dot, and a desktop notification additionally raises a macOS banner; the badge clears on focus. This is the channel [agentmux](https://github.com/lockyc/agentmux)'s Claude hooks feed instead of shelling out to `osascript`.

Deferred (see [`docs/FOLLOWUPS.md`](docs/FOLLOWUPS.md)): `cmd+\`` to cycle windows, ad-hoc `cmd+T`/`cmd+N` tabs/windows, a controlled libghostty **source** build (the vendored binary is a throwaway prebuilt, currently blocked on a Zig 0.15.2 / macOS 26 SDK mismatch), and `TerminalSurface` seam + IPC hardening.

## Config

`~/.config/warden/config.toml` (override with `WARDEN_CONFIG`):

```toml
shell = "fish -l"            # global default shell every tab spawns

[[window]]                   # a native macOS window
name   = "work"
colour = "#0f8a8a"
cmd    = "amux"              # this window's default startup command (each tab can override)

  [[window.tab]]             # a project terminal
  title      = "myproject"   # optional; defaults to the dir basename
  dir        = "~/code/myproject"
  keep_alive = true          # optional; spawn at launch and keep running

  [[window.tab]]
  title = "notes"
  dir   = "~/notes"
  cmd   = ""                 # opt out: just a bare shell here

  [[window.group]]           # optional: a labelled sidebar section
  name = "services"
    [[window.group.tab]]     # same fields as [[window.tab]]
    title = "api"
    dir   = "~/code/api"
```

A window has its own colour + name banner; its tabs are project terminals. Each tab opens a `shell`; a tab's `cmd` is auto-run *inside* that shell (it's typed in, not exec'd, so a shell function like [agentmux](https://github.com/lockyc/agentmux)'s `amux` works and you drop back to a live shell when it exits). Both `shell` and `cmd` **cascade** — set them globally, per-window, or per-tab, and the nearest level wins (`cmd = ""` opts a level out of an inherited command). `keep_alive` tabs start at launch and keep running in the background. Tabs can be **grouped** into labelled sidebar sections with `[[window.group]]`; loose `[[window.tab]]`s (no group) appear first in a headerless section. Grouping is cosmetic — it just sections the sidebar.

## Build & use

With [`just`](https://github.com/casey/just) (run `just` to list recipes):

```sh
just run          # launch the app against examples/config.toml (never touches your real config)
just validate     # validate the demo config (pass a path to validate another)
just test         # workspace tests
just fmt          # format
just clippy       # lint (warnings as errors)
just build        # build the release warden.app (needs: cargo install tauri-cli --version ^2)
just deploy       # build, install to /Applications (unsigned), and relaunch
```

`just deploy` produces a **non-notarized** local build (code-signed with your Apple Development identity if you have one, else unsigned) and strips the Gatekeeper quarantine xattr so it runs; it is not notarized for distribution to other machines.

Or with cargo directly:

```sh
cargo build
cargo test
cargo run -p warden-app                                # launch the app (macOS; reads WARDEN_CONFIG or ~/.config/warden/config.toml)
cargo run -p warden-config --bin warden -- validate    # validate ~/.config/warden/config.toml
cargo run -p warden-config --bin warden -- validate path/to/config.toml
```

`warden-app` materializes a window for each `[[window]]` and hot-reloads on save; edit the config while it's running to watch windows and tabs appear, disappear, and recolour live.

`warden validate` prints the resolved windows/tabs and any warnings; exit code 0 (ok), 1 (load/parse/validation error), 2 (usage).

## Layout

- `crates/warden-config/` — the config crate (library + `warden` CLI).
- `crates/warden-app/` — the macOS Tauri app: windows, the sidebar tab list, libghostty surfaces behind the `TerminalSurface` seam, and hot-reload wiring.
- `assets/` — icon masters (`icon.svg`, `icon-app.svg`), rendered PNGs, the macOS `warden.icns`, and `build-icons.sh` to regenerate the rasters from the SVGs.
- `docs/FOLLOWUPS.md` — tracked list of intentionally-deferred work.

## License

MIT — see [`LICENSE`](LICENSE).

The vendored libghostty binary (`crates/warden-app/vendor/`) is third-party code
distributed under its own MIT license (Ghostty); see
[`crates/warden-app/vendor/LICENSE-ghostty`](crates/warden-app/vendor/LICENSE-ghostty)
and `PROVENANCE.md` in that directory.
