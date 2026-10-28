---
type: architecture
links:
  - rel: part-of
    to: CLAUDE.md
    note: CLAUDE.md points here before any work on Rex-backed tabs
  - rel: references
    to: docs/surface.md
    note: a Rex pane is an ordinary libghostty surface; its size claims ride the surface's AppKit signals
---

# Rex-backed tabs — the hub, the relay and the pane mapping

Read before touching `crates/rex-client`, `crates/warden-relay`, `crates/warden-app/src/rex/`, or
anything a tab with a `server` does differently from a plain one. Each module's docblock carries its
own mechanism; this doc holds the map and the rules no single module owns.

## The normal case

Rex is Superlogical's server-side terminal multiplexer: a server keeps
sessions alive and clients attach to them. A tab whose config resolves a `server` (`"local"` for
this Mac's Rex server, or a host label from Rex's host list) is a **Rex tab**. Each of its panes —
the main pane and the split — is one Rex session on that server, and the pane's surface runs
`warden-relay` as its command instead of a shell. Everything else is a plain tab's path: the
sidebar, splits, pop-out, the `TerminalSurface` seam. Closing things only detaches, so reopening
shows the same live screen.

Plain tabs are a permanent kind, not a legacy path: Rex is optional and invite-only, and warden is
public. Rex is chosen per tab.

## The map

| Piece | Owns |
|---|---|
| `crates/rex-client` | The protocol: control connection (`ControlClient`), data streams (`DataStream`), wire codecs, `Endpoint`. Pins `API_REVISION` in `hello`; the server translates older revisions forward. `rex_client::fake` (feature `fake`) is the in-process fake server every gate test uses. |
| `crates/warden-relay` | One pane's block: stream → stdout (the opening snapshot decoded to VT with libghostty-vt at the pane's size, then raw output minus Rex's own messages), stdin → `terminal.write`, resume/restart on a dropped stream. |
| `rex/mod.rs` — `RexHub` | warden-app's one owner of Rex control connections: one per server, the live session lists, find-or-create per pane, block owners, claim/resize, destroy, `HubEvent`s. |
| `rex/store.rs` | The only Rex state warden keeps: pane key → {server, session id}, persisted per config file. |
| `rex/server.rs` | `server` value → `Endpoint` (host labels via the local server's `hosts.list`); starting a stopped local server. |
| `rex/reports.rs` | The Unix socket relays report to: one JSON line per report from the block's program, delivered as `AgentReport`s. |
| `rex/spawn.rs` | The relay command line, and carrying a pane's open off the main thread and back. |
| `rex/size.rs` | The size worker (`Sizer`) behind every claim and resize. |
| `rex/unclaimed.rs` | Sessions no tab claims, as sidebar rows under `Rex · <server>`. |
| `rex/windows.rs` | Each attached session's windows (new, close, relabel) and every terminal block's status: its root OSC 7501 record and its title, from Rex's events. |

Who holds which connection:

```
warden-app ── RexHub ── one control connection per server ──┐
   (session list, find/create, owners, claim + resize,      │
    destroy)                                                │  Rex server
pane surface ── warden-relay ── its own control connection ─┤  (local socket or
   --endpoint --session --block   (attach, terminal.write,  │   HTTPS)
                                   data.connect tickets)    │
                               └─ data stream ──────────────┘
```

The relay mints its own tickets, so the stream belongs to the relay's client and lives as long as
the relay does. The one channel between warden-app and a relay is the report socket
(`--report`, `rex/reports.rs`): a program in the block can write `ESC _ warden1;resume=<base64>
ESC \`, which `StreamDemux` strips from the output and the relay forwards as a JSON line
`{"session","block","resume"}`. A missing or unreachable socket drops the report; streaming
continues. The path must fit macOS's 104-byte `sun_path`.

The protocol's reference is the one each server serves at `GET /llms.txt` on its socket; Rex's docs
site omits it.

## Invariants across the pieces

- **A pane's Rex identity is its spec id** (`registry::rex_pane_key`): the tab key for the main
  pane, `registry::secondary_id` of it for the split, `rex:<session id>` for an unclaimed row
  (`unclaimed::is_unclaimed`). A runtime ⌘D split therefore reattaches the same session a config
  split would. Two windows holding the same tab key share one session.
- **warden keeps no shadow of Rex's state.** Beyond the store's pane → session mapping, everything —
  which sessions exist, who owns a block, whether a program exited — is read from Rex.
- **A pane shows one block:** the first terminal block of its session's first window. Windows made
  elsewhere (Rex.app, the phone) stay reachable from those clients.
- **`cmd` and the split's `cmd` are typed once, when warden creates the session**
  (`initial_input`); reattaching types nothing (`spawn::effective_spec` clears `startup`).
- **Size has one writer: warden-app.** A block's size is its owner's. warden-app claims
  (`own_block take`, then `resize`) when the user turns to a pane — focus gained in the key window,
  the window becoming key, a key or mouse press — taken from AppKit (`SurfaceSignal::UserInput`,
  see `docs/surface.md`), and resends the size on a frame change only while it owns the block.
  Whoever was last used owns the size, and focus *gain* is an edge, so two clients can't
  ping-pong. The relay never resizes and drops every terminal-generated reply
  (`rex_client::wire::strip_terminal_replies`): the server answers terminal queries itself, so a
  forwarded reply is a doubled answer and a spurious claim trigger.
- **AppKit's main thread never waits on the network.** Opens run on an opener worker
  (`spawn::opener`), claims on the `Sizer` worker, ☠ and exit checks on their own workers; the hub
  never holds its state lock across a network call.
- **An open answers within `OPEN_BUDGET`**, every step on one deadline: a pane opens or reports its
  server unreachable. Starting a stopped local Rex fits inside that budget, and **only a tab open
  starts Rex** — the launch/reload connect (to the servers `unclaimed::launch_servers` returns)
  never does, so a Rex the user quit stays quit.

## Lifecycle

| Event | The pane's Rex session | Where |
|---|---|---|
| ⌘W unload, closing a window, quitting warden | Detached: the relay exits, the session keeps running. The next open reattaches. | `Registry::unload` |
| ☠ destroy | Both panes' sessions destroyed. A Rex tab's `kill` is not run. | `EndTarget::Rex` → `spawn::destroy` |
| ⏻ suspend | Runs the `suspend` command on this Mac, so only a `"local"` Rex tab offers it. | `registry::suspend_cmd` |
| The pane's program exits (`exit`, an agent quitting) | Destroyed if its block was the session's last; otherwise the pane lets go and the session lists as unclaimed. The pane goes cold, and the next open creates a fresh session: an exited block would only leave a dead screen that reads as live. | `spawn::end_if_exited` → `RexHub::destroy_if_exited` |
| The server restarts, crashes, or the Mac reboots | Every mapped session is gone; each pane's next open creates a fresh one and types `cmd` again. | `RexHub::ensure_session` |
| Opening an unclaimed row | A temporary tab: adopted in memory, never persisted, never creates a session, can't split. ☠ ends the session. | `RexHub::adopt`, `Registry::split` |
| The relay's stream drops | Resumed at its exact byte count; if refused, a new ticket and a full reset + redraw from a fresh snapshot. | `warden-relay` `RESET` |
| The hub's control connection drops | Reconnected with the reconnect token; the session list is reread and claimed sessions reattached. | the hub's watcher |

**Presence.** A Rex tab's dot is whether its session is in its server's session list — the
last heard while the hub's connection is down: cyan or hollow, never a ghost. It never offers ▶
start, which types `cmd`: opening the tab is its start. Its `probe` is not run
(`docs/probing.md`).

**Remote servers.** `dir` is a path on the server (no `~` expansion, no existence check — see
`docs/config.md`); the relay itself runs locally from `/`.

## Why a relay, not a libghostty patch

`ghostty.h` feeds a surface only from its own child's PTY and cannot load a snapshot, so the
stream needs a local process in the PTY. The alternative — patching libghostty to feed a surface
from the network — puts a patch in Ghostty's termio internals, which have no stability promise, and
carries a fork through every revendor, security revendors included; upstream points outside
embedders at libghostty-vt, so the fork would be permanent. The relay costs one local PTY hop and
one more build artifact, and links libghostty-vt only, from the same pinned commit as GhosttyKit
(`docs/surface.md`). It is a separate binary so warden-app never links two libghostty builds.

Two other shapes are ruled out:

- **A text-mode attach client** (tmux, or Rex's removed `rex attach`) puts a second emulator in the
  path. A Rex pane has one parser in warden: the surface's libghostty.
- **The relay doing everything**, including size: it can't tell the user's keystrokes from the
  terminal's automatic replies and sees the surface size only through SIGWINCH, so size flaps.
  Hence the split above — warden-app sizes, the relay streams.

## Building and testing

The relay is found beside the running binary (`spawn::relay_path`): `just run` builds it first, and
the bundle carries it in `Contents/MacOS/` (`docs/releasing.md`). A bare `cargo run -p warden-app`
doesn't build it, and its Rex tabs then fail to open.

`just gate` never needs a real Rex install: the hub, relay and client tests run against
`rex_client::fake`. The real-server checks are `#[ignore]`d — run them with `cargo test -p <crate>
-- --ignored` against a running local Rex server; each touches only the sessions it labels and
destroys them.

Deferred work — remote roots, remote HTTPS, the agent tab list — is in `docs/FOLLOWUPS.md`.
