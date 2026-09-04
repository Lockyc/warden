---
type: decision
links:
  - rel: part-of
    to: CLAUDE.md
    note: the direction record CLAUDE.md's Intended architecture section points into
---

# Native splits & the smart-client direction

Why warden grows splits of its own, and what that makes possible. Read before adding a
terminal capability that a multiplexer would otherwise provide, or before assuming a
split has to come from tmux.

The external evidence — Superlogical's published architecture, transcribed because its
primary sources cannot be fetched by agent tooling — lives once, in
[agentmux's `docs/multiplexer-direction.md`](https://github.com/lockyc/agentmux/blob/main/docs/multiplexer-direction.md).
Facts here are as of 2026-09-04 and cover only what falls to warden.

## warden is already the shape the direction is heading toward

The `TerminalSurface` seam hosts **one embedded libghostty surface per PTY**, with tabs,
windows, banners and the pop-out detach all rendered as native chrome rather than painted
into a terminal grid. Superlogical's multiplexer describes the same split model
independently — native tabs/windows/splits, each with its own connection, one-to-one with
a PTY, and deliberately *no* multiplexing within a window. That convergence is the reason
this is a direction rather than a preference: warden does not need to become a
multiplexer, it needs to stop delegating to one.

**The unit of progress is the number of terminal emulators between warden and each PTY.**
An `amux --frame` tab is two (frame + agent); an unframed one is one. Native splits — two
surfaces in a tab, a shell beside the agent — removes the frame layer for local use and
takes that count to one, without agentmux losing `--frame` for the standalone, remote and
non-macOS cases it still serves.

What that buys beyond one less parse: native scrollback and selection in the second
surface, and a **per-client viewport**. Shared scroll across attached clients is tmux's,
not warden's, the moment the surface is warden's own.

## Inline images: the surface is warden's, and libghostty can now do it

An agent's own image output cannot be rendered inline in its TUI — the wall is inside the
agent, upstream of any multiplexer (mechanism and closed upstream issues: the agentmux
doc above). The only shape that works is **out-of-band**: a surface the agent's renderer
does not own. warden panes are exactly that, and are a better target than a tmux popup.

libghostty gained kitty graphics protocol support, which makes this a warden capability
with no multiplexer involvement at all. Three things shape how it must be wired:

- **PNG decoding is an optional, runtime-swappable `sys` callback**, not a built-in —
  libghostty keeps its no-runtime-dependency property, so the embedder supplies a decoder
  (a Rust PNG crate, here). Without one the protocol still works for direct RGB; only PNG
  transfers fail.
- **The embedder default image budget is conservative** (far below the Ghostty GUI's), so
  it must be raised deliberately rather than assumed.
- **File-based and shared-memory transfer mediums are opt-in**, because libghostty will
  not touch the filesystem without the embedder saying so. Enabling them is a decision
  with a blast radius, not a default to flip.

This lands behind the existing rule that libghostty's embedding C API is unstable and
pinned: the pin has to move to a rev carrying kitty graphics before any of it is
reachable.

## Not yet

**Rendering agentmux's status rows and notes as warden chrome.** Those are currently
painted into tmux's grid by `tmux-status.sh` / `notes.sh`; in a client-owned-viewport
model they belong to the client, and warden's sidebar, tab-row dots and presence
indicators are already that client. This is the expensive half of removing the *agent*
tmux layer and is what makes that layer removable at all. Blocked on agentmux's session
backend seam existing; revisit once it does — not before, or warden ends up owning chrome
for a backend that is still the only one.
