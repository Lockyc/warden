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

## Inline images: the surface is warden's, and it already renders them

An agent's own image output cannot be rendered inline in its TUI — the wall is inside the
agent, upstream of any multiplexer (mechanism and closed upstream issues: the agentmux
doc above). The only shape that works is **out-of-band**: a surface the agent's renderer
does not own. warden panes are exactly that, and are a better target than a tmux popup.

**The capability is live now, at the current pin, with no warden code.** Verified 2026-09-04
against the vendored `35e1a01`: a kitty graphics escape written to a warden tab renders
inline for both PNG (`f=100`) and direct RGB (`f=24`). warden embeds Ghostty's own surface
and renderer, which have carried the protocol all along, and the vendored `libghostty.a`
statically links libpng. Nothing here is gated on moving the pin, and there is no image
budget, decoder or transfer-medium wiring for warden to do — it inherits Ghostty's defaults.

- **Footgun: upstream's `include/ghostty/vt/kitty_graphics.h` and `vt/sys.h` are a DIFFERENT
  library and do not apply to warden.** They belong to **libghostty-vt**, the standalone VT
  parser, whose kitty API is a *query* surface (placement iterator, image pixel data,
  geometry) for an embedder writing its own renderer — and whose `sys.h` is where the
  swappable PNG-decode callback, the conservative image budget and the opt-in file/shm
  mediums live. warden consumes the *embedding* API (`include/ghostty.h`,
  `ghostty_surface_*`), which carries no image symbols at all. Reading vt's headers as
  warden's contract yields a wiring task that does not exist.

**Through tmux, images transit but are not managed** (measured, same session). With
`allow-passthrough on`, a kitty escape wrapped in tmux's DCS (`ESC P tmux; …`, inner `ESC`
doubled) renders; the same escape unwrapped is swallowed, with the control string leaking
into the window title. But tmux's grid has no idea the pixel region is occupied — the next
line of text draws straight over the image. So a multiplexer hop cannot own image
placement across scroll, reflow or redraw, which is why the out-of-band surface has to be
warden's own rather than a tmux popup.

## Not yet

**Rendering agentmux's status rows and notes as warden chrome.** Those are currently
painted into tmux's grid by `tmux-status.sh` / `notes.sh`; in a client-owned-viewport
model they belong to the client, and warden's sidebar, tab-row dots and presence
indicators are already that client. This is the expensive half of removing the *agent*
tmux layer and is what makes that layer removable at all. Blocked on agentmux's session
backend seam existing; revisit once it does — not before, or warden ends up owning chrome
for a backend that is still the only one.
