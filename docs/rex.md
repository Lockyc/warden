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
| `rex/layout.rs` | A session window's split tree (`Layout`) as tiles and dividers for the page, and a config pane stack as the layout a new session is created with (`stack_layout`). |
| `rex/column.rs` | A split pane's scratch column: one session whose windows are scratch tabs and whose split trees are the stacked panes — opening it (created from the config stack, retyped when restored), new tab, split, divider resize, closing a block or a tab, each block's size key. |
| `rex/agents.rs`, `rex/agent_open.rs` | Agent records (group, order, resume, asleep/closed) and folded groups, and opening a main pane: which agents are live. |
| `rex/lifecycle.rs` | The agent tab list: new, wake, sleep, close, reopen, fork, rename (`RexHub::agent_op`), which agent shows (`settle`, an open's too), following a pane's windows and statuses, storing reported resume commands, and the list `warden:agents` carries. |
| `rex/attention.rs` | Which agent tabs need you (blocked, or done and unseen): a group's amber, an agent row's unread, and the row badge and banner a hidden one raises (`notify::raise`). |

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
(`--report`, `rex/reports.rs`). An agent's hook (`scripts/agent-hooks/claude-hook.sh`,
wired by `/warden:install`) writes `ESC _ warden1;resume=<base64> ESC \` to the agent's terminal;
`StreamDemux` strips it from the output and the relay forwards it as a JSON line
`{"session","block","resume"}`. Any program in the pane can write that sequence, and a woken agent's
window types the stored command into a shell, so a report is untrusted: `AgentStore::report_resume`
keeps only a command matching `agents::RESUME_SHAPES`, and loading checks it again. A missing or
unreachable socket drops the report; streaming continues.

The protocol's reference is the one each server serves at `GET /llms.txt` on its socket; Rex's docs
site omits it.

## Invariants across the pieces

- **A pane's Rex identity is its spec id** (`registry::rex_pane_key`): the tab key for the main
  pane, `registry::secondary_id` of it for the split, `rex:<session id>` for an unclaimed row
  (`unclaimed::is_unclaimed`); an agent's is `registry::agent_key`, bound to its block by the
  open. A split removed from the config and added back therefore reattaches its session. Two
  windows holding the same tab key share one session.
- **Rex owns windows; warden owns records.** Which sessions and windows exist, who owns a block,
  whether a program exited and each block's status are read from Rex. warden keeps only what Rex
  can't: the store's pane → session mapping and the agent records (`rex/agents.rs` — group, order,
  resume command, asleep or closed), reconciled against Rex's windows whenever they change
  (`agents::reconcile`).
- **Agent status comes from the hub, never a relay** (`program_status_changed`, `title_changed`;
  `rex/windows.rs`), so an agent tab that isn't on screen still raises its project row's badge and
  a banner, and ambers its group while folded (`rex/attention.rs` → `notify::raise`). The summary
  block's rows ride the same path: *doing* is the root record's `msg`, and *asked* / *said* are
  OSC 7501 child records the hook writes straight to Rex (`warden/asked`, `warden/answered`,
  `warden/said`; which prompt is which: `scripts/agent-hooks/claude-hook.sh`), so they reach
  warden for an agent no relay streams, and on a remote server. *title* is the row's name.
- **A main pane's session windows are its agent tabs**, one surface each — the window's first
  terminal block — with only the selected one on screen. A split pane's session windows are its
  scratch tabs, each showing every block of its split tree (`docs/native-splits-direction.md`).
  An unclaimed row shows one block: the first terminal block of its session's first window.
- **`cmd` and each split pane's `cmd` are typed once, when warden creates the session**
  (`initial_input`); a woken agent's window types its resume command instead. Reattaching types
  nothing (`spawn::effective_spec` clears `startup`).
- **No open wakes an agent** — only selecting its row does (`lifecycle::select`). An open
  attaches what is live and leaves a pane holding only wakeable agents blank; a pane with nothing
  live and nothing to wake it loads.
- **Size has one writer: warden-app.** A block's size is its owner's. warden-app claims
  (`own_block take`, then `resize`) when the user turns to a pane — focus gained in the key window,
  the window becoming key, a key or mouse press — taken from AppKit (`SurfaceSignal::UserInput`,
  see `docs/surface.md`), and resends the size on a frame change only while it owns the block.
  Whoever was last used owns the size, and focus *gain* is an edge, so two clients can't
  ping-pong. The relay never resizes and drops each terminal-generated reply a read
  carries whole (`rex_client::wire::strip_terminal_replies`): the server answers terminal queries itself, so a
  forwarded reply is a doubled answer and a spurious claim trigger.
- **AppKit's main thread never waits on the network.** Opens run on an opener worker
  (`spawn::opener`), claims on the `Sizer` worker, ☠ and exit checks on their own workers; the hub
  never holds its state lock across a network call.
- **An open answers within `OPEN_BUDGET`**, every step on one deadline: a pane opens or reports its
  server unreachable. Starting a stopped local Rex fits inside that budget, and **only a tab open
  starts Rex** — the launch/reload connect (to the servers `unclaimed::launch_servers` returns)
  never does, so a Rex the user quit stays quit.
- **A change to an open scratch column waits out a slow Rex** (`column::CHANGE_BUDGET`, not
  `OPEN_BUDGET`): a sent request is carried out however late Rex answers. A column takes one new
  tab / split / tab close at a time (`Registry::begin_column_change`); Split and New Scratch Tab
  show the reason meanwhile.

## Lifecycle

| Event | The pane's Rex session | Where |
|---|---|---|
| ⌘W unload, closing a window, quitting warden; the split removed from the config (its column's session) | Detached: the relay exits, the session keeps running. The next open reattaches. | `Registry::unload`, `Registry::unsplit` |
| ☠ destroy | Both panes' sessions destroyed; the tab's resumable agents go to the closed fold, the rest drop. A Rex tab's `kill` is not run. | `EndTarget::Rex` → `spawn::destroy` |
| An agent tab sleeps or closes | Its window closes; the record stays asleep, or goes to the closed fold, to wake later typing its resume command. One that never reported a resume command can't sleep, and closes for good only once confirmed. Closing the last leaves the session empty — Rex keeps it — and the next agent tab opens in it (`RexHub::ensure_until`); the tab stays open while its scratch column runs. | `lifecycle::run` → `RexHub::agent_op` |
| An agent tab forks | A new window types its fork command (`agents::fork_command`: the resume command plus the harness's fork words), as a new record directly below it in its group; the source is left as it is, live or asleep. The fork reports its own resume command when its hook runs. One without a resume command, or whose harness has no fork form, can't fork. | `AgentOp::Fork` |
| A window opens or closes elsewhere (Rex.app, the phone) | Appended as a tab, ungrouped; a closed one goes to the closed fold if resumable, else drops, and the selection leans to a live neighbour. | `lifecycle::watch_agents` → `AgentOp::Refresh` |
| Tab ▸ Sleep Server | Every resumable live agent on the server sleeps (window closed, record asleep, one at a time), then `server.stop`; every pane on it goes cold (a popped-out one ends) and the hub lists the server with no sessions and doesn't reconnect; Rex saves the sessions, so its next start restores them (the restart row). Agents with no resume command and sessions no pane maps are confirmed by name first (scratch columns end unasked), and again if one appears before the stop. Rex takes `server.stop` only on its local socket, so a server reached over HTTPS can't sleep. | `lifecycle::sleep_server` → `RexHub::sleep_server` |
| ⏻ sleep project | Sleep Server narrowed to one tab: its surfaces let go, its resumable agents sleep, agents with nothing to resume are confirmed by name, then both panes' sessions are destroyed — the scratch column's unasked. The records stay asleep. Its `suspend` is not run. | `lifecycle::sleep_project` → `RexHub::sleep_project` |
| The pane's program exits (`exit`, an agent quitting) | Destroyed if its block was the session's last; otherwise the pane lets go and the session lists as unclaimed. The pane goes cold, and the next open creates a fresh session: an exited block would only leave a dead screen that reads as live. An agent's exit closes only its window (the session's last ends as above) and its record closes or drops; the pane shows a live neighbour, else empties — and the tab goes cold unless its scratch column runs. | `spawn::end_if_exited` → `RexHub::destroy_if_exited` / `end_agent_if_exited` → `AgentOp::Exited`; `Registry::close_agent` |
| A scratch column block's program exits, or Close Pane closes it, or the strip's × closes its scratch tab | Only that block (or that window's blocks) closes: its window reflows, or closes with it as its last block; Rex keeps the session when its last window closes, and a column found holding no block — on its next view or its next open — gets a fresh scratch tab (a bare shell in the tab's `dir`), so the split stays. | `spawn::end_if_exited` / `spawn::close_block` → `RexHub::column_close_block`; `RexHub::column_close_tab`; `lifecycle::follow_column` → `RexHub::column_fill` |
| The server restarts, crashes, or the Mac reboots | Rex restores every session with the same ids, each terminal a bare shell. A record from an earlier server instance reads as gone (hollow, or the ghost with agents to wake) until an open adopts its session: the open types `cmd` into the restored shell; a main pane's resumable agents sleep and their windows close (the session too, if nothing else was in it), the rest drop, and the pane stays blank until a row's click wakes one. A tab open when Rex restarts lets go as under Sleep Server (docked cold, popped-out ended). | `State::restored`, `RexHub::ensure_session`, `retire_restored`, `spawn::reopen_restarted` |
| Opening an unclaimed row | A temporary tab: adopted in memory, never persisted, never creates a session, can't split. ☠ ends the session. | `RexHub::adopt`, `Registry::split` |
| The relay's stream drops | Resumed at its exact byte count; if refused, a new ticket and a full reset + redraw from a fresh snapshot. | `warden-relay` `RESET` |
| The pane or its block changes size | Once the size settles, the relay ends its stream and redraws from a fresh snapshot fitted to the pane — output laid out for one size garbles a pane of another. | `warden-relay` `Redraw` |
| The hub's control connection drops | Reconnected with the reconnect token; the session list is reread and claimed sessions reattached. | the hub's watcher |

**Presence.** A Rex tab's dot is whether either pane's session (main or scratch column,
`RexHub::tab_alive`) is in its server's session list — the
last heard while the hub's connection is down: cyan; else a ghost while it holds asleep agents to
wake (`AgentStore::has_wakeable`); else hollow. It never offers ▶
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

Deferred work — remote roots, remote HTTPS, the closed set of resume shapes — is in
`docs/FOLLOWUPS.md`.
