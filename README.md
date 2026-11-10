<img src="assets/icon-1024.png" alt="warden" width="128" height="128">

# warden

[![Release](https://img.shields.io/github/v/release/lockyc/warden?sort=semver&label=release)](https://github.com/lockyc/warden/releases/latest)
![Platform](https://img.shields.io/badge/platform-macOS-555)
![Rust](https://img.shields.io/badge/rust-CE412B?logo=rust&logoColor=white)
![Tauri](https://img.shields.io/badge/Tauri%20v2-24C8DB?logo=tauri&logoColor=white)
[![License](https://img.shields.io/github/license/lockyc/warden)](LICENSE)

A terminal app that keeps every project open where you left it. One TOML file declares windows —
say, work and personal — and the project tabs inside them; warden builds itself from that file
and reloads when you save it. A tab runs whatever you give it: a shell, a build watcher, a TUI or
a coding agent. Agents on a Rex server (Superlogical's server-side terminal multiplexer) keep
running when warden quits, and Rex is optional: a tab without one is a plain terminal.

![Two warden windows, personal behind work. The work window: the sidebar of grouped project tabs, the planning tab's agent list (one needs you, one asleep), a Claude Code agent's plan in the main pane with the asked/doing/said summary below it, and a scratch shell column on the right](docs/screenshot.png)

## Install

**Download:** grab `warden-<version>-macos.zip` from the
[latest release](https://github.com/lockyc/warden/releases/latest), unzip, and move `warden.app`
to `/Applications`. Releases are signed and notarized, and run natively on Apple Silicon and
Intel.

**Or build from source** into `~/.warden` (needs the Xcode Command Line Tools and a Rust
toolchain from [rustup](https://rustup.rs); the installer adds the Tauri CLI). It also seeds
`~/.config/warden/config.toml` and copies the agent hook to `~/.local/share/warden/agent-hooks/`;
re-run it to update:

```sh
curl -fsSL https://raw.githubusercontent.com/lockyc/warden/main/install.sh | bash
```

**Agent tabs' sleep, fork and summary** need that hook wired into Claude Code, which
`/warden:install` does: run it in Claude Code from a clone of this repo. The hook needs `jq`.

warden updates itself: on launch, every 6 hours, and from **warden ▸ Check for Updates…**, an
*Update available* bar offers **Update & Relaunch**. Nothing installs without your approval, and
each update's signature is checked first.

## Features

Each in full, with every key and shortcut: [docs/features.md](docs/features.md).

- **Windows and project tabs from one file.** Each `[[window]]` is a native window with a colour
  and title banner; each tab is a real terminal in a project's directory. Edits apply live on
  save, and closing the last window doesn't quit warden.
- **Project trees** — point a `[[window.root]]` at `~/Developer` and every git repo under it
  becomes a tab, with no per-project config.
- **Rex sessions.** With `server = "local"`, a tab's terminals live on a Rex server, so closing
  the window or quitting warden only detaches; reopening shows the same live screen.
- **Agent tabs.** A Rex tab lists its coding agents beside the main pane, with a summary of what
  the selected one was asked, is doing and last said. Agents sleep and wake with their
  conversation, fork into a separate branch, and badge the tab when they need you.
- **Splits** — a second pane beside a tab's terminal, declared in the config or added with **⌘D**;
  on a Rex tab, a column of scratch shells.
- **Pop out** a tab (**⌘⇧O**) into its own window with its terminal, scrollback and process
  intact.
- **Session dots for plain tabs** — a `probe` command (say, `tmux has-session -t {title}`) shows
  whether a tab's session is running, restorable or gone, and the dot restarts it in place.
- **Clickable links**, screened first: ⌘-click opens web and mail links, asks before a custom
  scheme, and refuses anything that could run code.
- **Notifications** — a background tab that rings the bell or sends a desktop notification gets an
  amber badge and, for a notification, a macOS banner.
- **Sidebar search** (**⌘⇧F**), a collapsible sidebar (**⌃⌘S**), and **⌘1–⌘9** to jump between
  tabs.

## Configuration

`~/.config/warden/config.toml` (override with `WARDEN_CONFIG`). Full schema, validation rules and
resolution semantics: [docs/config.md](docs/config.md).

```toml
shell = "fish -l"            # global default shell
format_on_save = true        # rewrite this file tidy on each clean save
density = "compact"          # condensed chrome

[[window]]                   # a native macOS window
title  = "work"
colour = "#0f8a8a"           # banner accent
width  = 1500                # initial size, px
height = 1000
server = "local"             # every tab here keeps its terminals in sessions on this Mac's Rex server
cmd    = "claude"            # the agent each tab starts (+ New tab starts another)

  [window.split]             # a column of scratch shells beside each tab's agents
  side = "right"
  size = 0.3

  [[window.tab]]             # a project tab
  title      = "myproject"   # defaults to the dir basename
  dir        = "~/code/myproject"
  load_on_open = true        # spawn at launch and keep running

  [[window.tab]]
  title  = "notes"
  dir    = "~/notes"
  server = ""                # opt out: a plain terminal, not a Rex session
  cmd    = ""                # and just a bare shell

  [[window.group]]           # optional: a labelled sidebar section
  name = "services"
    [[window.group.tab]]     # same fields as [[window.tab]]
    title = "api"
    dir   = "~/code/api"

  [[window.root]]            # optional: scan a dir; every git repo under it becomes a tab
  name = "Developer"
  dir  = "~/Developer"
```

**Global** (top of the file):

| Key | Default | What it does |
| --- | --- | --- |
| `shell` | your login shell, run as a login shell | The shell every tab spawns. Cascades. |
| `cmd` | none | Command typed into that shell on spawn. Cascades. |
| `probe` | none | Session-presence check, run per tab (cwd = the tab's dir): exit 0 ⇒ cyan dot, exit 3 ⇒ ghost (restorable), anything else ⇒ hollow. Cascades. |
| `probe_interval` | `5` | Slow-poll floor in seconds once a burst settles. `0` = event-driven only (still bursts on triggers, no steady poll). |
| `kill` | none | Session-destroy command (☠), run on the presence dot's confirm row; the tab then unloads; ⌘⇧K runs it even on a tab without `probe`. Cascades. |
| `suspend` | none | Session-suspend command (⏻, restorable), run on the presence dot's confirm row; the tab then unloads; ⌘⇧S runs it even on a tab without `probe`. Cascades. |
| `resume` | none | Program that replaces the first word of an agent tab's reported resume command, e.g. `"my-claude"` types `my-claude --resume X` instead of `claude --resume X`. Cascades; `""` opts a level out. |
| `server` | none | Keep every tab's panes in sessions on a Rex server: `"local"` or a host label. Cascades; `""` opts a level out. On a remote server `dir` is a path on that host (no `~` expansion, no existence check). A window with a Rex tab also lists, under `Rex · <server>`, the sessions no tab of yours claims on every server warden is connected to; opening one attaches it until you close it, and nothing is written to config. |
| `split` | none | Give every tab a second pane. A table: `side` (`"left"`/`"right"`, default right), `size` (the second pane's share, `0.1`–`0.9`, default `0.5`), `cmd` (typed into it; absent = bare shell), or `pane` (below) instead of `cmd`. Each key inherits separately (`cmd`/`pane` as one); `true` inherits every key, `false` opts a level out. |
| `split.pane` | none | A Rex tab's split as a column of stacked scratch panes, top to bottom — `[[split.pane]]` entries (at most 8) in place of `cmd`. Each takes `cmd` (typed into it; absent = bare shell) and `size` (its share of the column, `0.05`–`0.9`); unsized panes share what's left equally. Applied when the session is created. A tab with no `server` runs only the last pane, with a warning. |
| `format_on_save` | `false` | Rewrite this file in house style on each clean save (same formatting as `warden fmt`). |
| `density` | `"comfortable"` | Chrome sizing. `"compact"` scales type + spacing down proportionally for denser tab lists. |
| `tab_digit_keys` | `"jump"` | ⌘1–⌘9 jump to a tab position. `"cycle"` makes ⌘1 / ⌘2 cycle next/prev instead, shifting jumps to ⌘3–⌘9. |
| `sidebar_drag` | `true` | The non-interactive sidebar chrome doubles as a window-move drag handle. |
| `open_tabs_section` | `false` | Pin an **Open** section above the main list, mirroring every tab that currently has a terminal open (including one popped out into its own window). Mirrored tabs keep their row in their own group or tree, so the main list never shuffles as terminals come and go; ⌘1–⌘9 and tab cycling ignore the mirrors. The section grows to 2/5 of the window, then scrolls on its own, independent of the main list. While the section is expanded, the main list below it is dimmed until you hover it; its header collapses it. Cascades global → window. |
| `background_tabs_section` | `true` | Pin a **Background** section, below **Open**, mirroring every unloaded tab whose `probe` reports a running (cyan dot) or restorable (ghost dot) session. Collapsed until you open it from its header; each window remembers which way you left it. Opening a tab from it reattaches or restores the session and moves the tab to **Open**. Cascades global → window. |
| `remember_tabs` | `false` | Each window comes back the way you left it: the tabs that had a terminal open respawn when it opens (a Rex tab reattaches its sessions; a plain tab's `cmd` re-runs), and the tab you were on is selected. Applies at launch and when you reopen a closed window. Cascades global → window. |
| `auto_update` | `true` | Check for a new release on launch and every 6h. `false` suppresses the auto-check; **Check for Updates…** still works. Takes effect at next launch. |
| `trusted_link_schemes` | `[]` | Custom URL schemes whose terminal links open on ⌘-click without asking first, e.g. `["vscode"]`. `http`, `https`, `mailto` and `file` can't be listed. |
| `notify_debug` | `false` | Trace the notification path to `$TMPDIR/warden-notify-dbg.log` — a debug aid, read at launch (see [`docs/notifications.md`](docs/notifications.md)). |

**`[[window]]`** — one native macOS window each:

| Key | Default | What it does |
| --- | --- | --- |
| `title` | *required* | Banner text + window title; unique across the config. Changing it in the file is destructive — the window is closed and reopened, so its terminals and saved size/position reset; **Edit Window…** on the banner renames it in place instead. |
| `colour` | neutral | Banner accent, `#rgb` or `#rrggbb`. |
| `width` / `height` | `1500` / `1000` | Initial size in px; the window's saved size/position wins after the first launch. |
| `open_on_start` | `true` | Materialize this window at launch. `false` = configured but closed — open it from the home surface or the **Window** menu, or script it with `open 'warden://open?window=<title>'`. |
| `open_tabs_section` | inherited from global | Whether THIS window's sidebar pins the **Open** section. Set it per window to show the section in a busy window and not a quiet one. |
| `background_tabs_section` | inherited from global | Whether THIS window's sidebar pins the **Background** section. |
| `remember_tabs` | inherited from global | Whether THIS window restores its loaded tabs and selection. |
| `shell` / `cmd` / `probe` / `kill` / `suspend` / `resume` / `server` / `split` | inherited from global | Per-window overrides for every tab in it. |

**`[[window.tab]]`** (and `[[window.group.tab]]`) — one project terminal each:

| Key | Default | What it does |
| --- | --- | --- |
| `dir` | *required* | Working directory the terminal opens in (`~` expanded; a relative path is relative to the config file). A dir that doesn't exist is a warning, not an error. On a remote `server` it's a path on that host, taken as written. |
| `title` | basename of `dir` | Display label. Purely cosmetic — may repeat within a window. |
| `id` | unset | Stable identity, needed only to disambiguate two tabs that share a `dir`. Otherwise the `dir` *is* the identity. |
| `load_on_open` | `false` | Spawn at launch and keep running in the background. Otherwise a tab spawns lazily on first focus. |
| `colour` | derived from `title` | The project colour — the agent pane's summary block, shade and identity tile — `#rgb` or `#rrggbb`. Unset, each title gets its own hue. |
| `shell` / `cmd` / `probe` / `kill` / `suspend` / `resume` / `server` / `split` | inherited from the window | Per-tab overrides. |

**`[[window.group]]`** — a labelled sidebar section:

| Key | Default | What it does |
| --- | --- | --- |
| `name` | *required* | Section header. Unique within the window (one namespace shared with `[[window.root]]` names). |

**`[[window.root]]`** — a scanned projects dir; every git repo found becomes a tab:

| Key | Default | What it does |
| --- | --- | --- |
| `dir` | *required* | Dir to scan (`~` expanded; a relative path is relative to the config file). The walk stops at each `.git` (never descends into a repo) and skips hidden dirs and symlinks. |
| `name` | basename of `dir` | Section header for the discovered tree. |
| `depth` | `6` | How deep to scan; must be ≥ 1. |
| `shell` / `cmd` / `probe` / `kill` / `suspend` / `resume` / `server` / `split` | inherited from the window | Overrides applied to every project discovered under this root. |
| `[[window.root.tab]]` | none | Overrides for one discovered project, matched by `dir` (`~` expanded; a relative path is relative to the config file). The project stays in the root's tree and inherits the root's values; a `dir` that doesn't exist warns. |
| `[[window.root.tab]]` `dir` | *required* | The project's dir. |
| `[[window.root.tab]]` `shell` / `cmd` / `probe` / `kill` / `suspend` / `resume` / `split` | inherited from the root | Per-project overrides; `split` inherits per key. |

Three rules the tables can't carry. **`cmd` is typed *into* the shell, not exec'd** — so a shell function or alias resolves, and you drop back to a live prompt when it exits. **The cascading keys resolve nearest-level-wins** — a tab's value beats its window's, which beats the global one, with `""` opting a level out of an inherited value (`cmd = ""` gives you a bare shell under a global `cmd`); projects discovered under a `[[window.root]]` have no tab level of their own, so the root takes the tab's place (root, then window, then global), unless a `[[window.root.tab]]` names the project (that, then root, window, global). `split` cascades per key — a tab table that sets only `size` keeps the window's `side` and `cmd` — and `split = false` is its `""`. And **grouping is cosmetic** — `[[window.group]]` only sections the sidebar; loose `[[window.tab]]`s appear first in a headerless section.

Everything above hot-reloads on save, except `auto_update` and `notify_debug` (read at launch).
`warden validate [path]` prints the resolved windows and tabs with any warnings, and
`warden fmt [--check] [path]` rewrites a config in house style.

## Limits

Rex is an invite-only beta, and remote Rex servers are untested. A Rex scratch column holding
more than one shell can't pop out. Ad-hoc ⌘T / ⌘N tabs and windows for plain (non-agent) tabs
aren't built. All tracked in [docs/FOLLOWUPS.md](docs/FOLLOWUPS.md).

## Status

In use, `0.x`. What's next: [docs/FOLLOWUPS.md](docs/FOLLOWUPS.md).

## Related

warden's siblings share its sidebar, config and app shell:
[curator](https://github.com/lockyc/curator) — browser tabs,
[lector](https://github.com/lockyc/lector) — local doc sites,
[flotilla](https://github.com/lockyc/flotilla) — document workspaces.
Its terminals are [libghostty](https://github.com/ghostty-org/ghostty), built by
[libghostty-build](https://github.com/lockyc/libghostty-build).

## Development

Needs the Tauri CLI (`cargo install tauri-cli --version ^2`) and [`just`](https://github.com/casey/just).

```sh
git clone https://github.com/lockyc/warden && cd warden
just hooks        # once per clone: enable .githooks
just run          # launch against examples/config.toml (never touches your real config)
just test         # workspace tests
just gate         # the full pre-merge gate
just install      # build, install to /Applications, and relaunch
```

`examples/config.toml` points at the mock project tree in
[`examples/projects/`](examples/projects/README.md). A source build is ad-hoc signed; `just install`
re-signs it with your *Developer ID Application* certificate when one is in your keychain, which
on macOS 26 avoids slow command launches. warden builds on three pinned shared crates —
[chrome-core](https://github.com/lockyc/chrome-core), [config-core](https://github.com/lockyc/config-core)
and [shell-core](https://github.com/lockyc/shell-core). The crate layout, working against a local
core and the release process: [CLAUDE.md](CLAUDE.md) and [docs/releasing.md](docs/releasing.md).

## License

MIT — see [LICENSE](LICENSE).

The vendored libghostty binaries (`crates/warden-app/vendor/`, and libghostty-vt in
`crates/warden-relay/vendor/`) are [Ghostty](https://github.com/ghostty-org/ghostty) compiled from
an unmodified, pinned upstream commit by
[`lockyc/libghostty-build`](https://github.com/lockyc/libghostty-build) and distributed under
Ghostty's MIT license; see
[`crates/warden-app/vendor/LICENSE-ghostty`](crates/warden-app/vendor/LICENSE-ghostty) and
[`PROVENANCE.md`](crates/warden-app/vendor/PROVENANCE.md) in that directory.
