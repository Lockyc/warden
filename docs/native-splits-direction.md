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
windows, banners, native splits, and the pop-out detach all rendered as native chrome
rather than painted into a terminal grid. Superlogical's multiplexer describes the same
split model independently — native tabs/windows/splits, each with its own connection,
one-to-one with a PTY, and deliberately *no* multiplexing within a window. That
convergence is the reason this is a direction rather than a preference: warden does not
need to become a multiplexer, it needs to stop delegating to one.

**The unit of progress is the number of terminal emulators between warden and each PTY.**
An `amux --frame` tab is two (frame + agent); an unframed one is one. Native splits — a
second surface beside a tab's primary, a shell beside the agent — remove the frame layer
for local use and take that count to one, without agentmux losing `--frame` for the
standalone, remote and non-macOS cases it still serves.

What that buys beyond one less parse: native scrollback and selection in the second
surface, and a **per-client viewport**. Shared scroll across attached clients is tmux's,
not warden's, the moment the surface is warden's own.

## Native splits: shipped

A tab holds a **primary pane plus an optional secondary** — two panes, not N-way. A split
comes from one of two places, and which one decides where its geometry is remembered:

- **Declared in config** (`split` — cascading global → window → tab, and root → window →
  global for discovered projects; schema in `docs/config.md`). The tab is created with its
  second pane already declared, both spawn together (also under `load_on_open`), the
  second pane runs `split.cmd` (bare shell when absent) in the tab's own shell and dir, and
  `split.size`/`split.side` seed the divider. A drag overrides the ratio **in memory only**,
  for as long as the tab's terminals are live: unload, a child exit, a hot-reload respawn
  and an app restart all bring the config ratio back. `exit` in the second shell closes the
  pane for the same lifetime — `Registry::unload` re-declares it, so it is back on the next
  spawn — and ⌘D recreates it from config. This is the frame-less shape of an `amux --frame` tab: the
  scratch shell beside the agent, with warden owning the split instead of tmux.
- **Created at runtime** (⌘D / Tab ▸ Split) on a tab with no config split. Its ratio and
  existence persist per tab in `localStorage` (keyed on window label + tab id, same home as
  sidebar width) and are restored on the next activation. A config-split tab never touches
  that store — the config is its source of truth — and a stale key for one is evicted.

Both are closed only by the secondary shell exiting (`exit` in it), and resized by dragging
the divider — a 1px line in a 5px grab strip that carries no close control, by choice: the
one route out of a split is the shell's own end, docked or popped out alike. ⌘W (unload) and ⌘⇧O (pop-out) both act on the **whole
tab** — unloading drops both panes to cold together, and popping out carries both surfaces
to the detached window at the tab's current effective ratio
(`shell_core::detach::DetachSpec.panes`, generic on purpose: a `Vec<f64>` of hole ratios,
not a `warden` split type, so a future consumer can divide its own hole without learning
warden's split concept).

Mechanism: `warden-config`'s `Tab.split: Option<Split>` (`resolve.rs::resolve_split_level` +
`cascade_split`; `reconcile.rs` routes a presence/`cmd` change to `respawn_tabs` and a
`side`/`size` change to `set_meta`), `Registry`'s `Pane`/`PaneIdx`/`TabSlot` model
(`crates/warden-app/src/registry.rs` — `add` declares a config split's second pane, one
`secondary_spec` derives its startup), `TabDto.split_layout` → the chrome's
`splitLayoutById`/`ratioOverride`, the `split_pane`/`focus_pane` commands
(`main.rs`), and the chrome's pane/divider DOM + `setSplitVisible` (`crates/warden-app/ui/index.html`). The
traps this shipped are catalogued in `CLAUDE.md`'s *Conventions & footguns* — read those
before touching split visibility, routing, or the backstop.

## Verifying splits by eye

warden's chrome has no automated test harness — splits are rendering behaviour, verified
only by eye. Each step below is written to actually fail if the feature regresses.

- An unsplit tab is visually unchanged from before splits existed — no divider, no second
  hole, no layout shift.
- ⌘D splits the active tab; a second ⌘D on an already-split tab is a no-op (does not reset
  a dragged ratio).
- The divider drags smoothly and both terminals track it live, not just on release.
- The divider renders as a single 1px line with no control on it, and the cursor turns to
  col-resize over the few pixels either side of it.
- Typing `exit` in the scratch (secondary) pane collapses the split; the primary fills the
  whole hole with no uncovered region at any point during the collapse, while the tab stays
  live — docked, and inside a popped-out window alike (the detached window drops to one hole;
  the origin forgets the persisted ratio).
- A popped-out tab whose primary shell exits comes home cold: the detached window closes,
  the origin row goes hollow (click to respawn), and the sidebar leans to a live neighbour
  exactly as a docked exit does.
- The ratio survives an app restart (persisted in `localStorage`, keyed per tab).
- Popping out an unsplit tab is unchanged — one hole, no `panes` in the detach payload.
- Popping out a split tab carries both panes into the detached window at the same ratio,
  with the same divider and focus ring; `exit` in the second pane closes it there too, and
  clicking either pane moves the ring.
- Closing the detached window returns both panes to the origin, still live.
- Arrow keys pressed in the second pane stay in it: the cursor, the focus ring and the
  keystroke all remain on the pane you typed in (arrows travel a different AppKit route
  from letters — `WardenHostView::owns_window_keys` in `CLAUDE.md`'s footguns).
- **The focused-pane marker** — the accent border — follows every click, into live terminal
  content and onto a cold pane's backstop alike, and after switching tabs away and back it
  is on the pane you last typed in. A popped-out tab shows no marker in its origin window
  (its panes are elsewhere).
- A tab under a `[split] side = "left" size = 0.3` block opens already split, the second
  pane on the LEFT at 30% of the hole, the primary on the right running the tab's `cmd`,
  with focus (the accent ring) on the primary.
- Resizing the window keeps the 30/70 proportion.
- Dragging the divider changes it; ⌘W then re-clicking the tab brings back 30/70. So does
  `exit` in the primary, and an app restart.
- `exit` in a config split's second pane closes it; ⌘W then re-clicking the tab brings it
  back; ⌘D on the closed split brings it back immediately, still on the left at 30%.
- Editing `size` in the config re-lays out the live split on save without respawning either
  terminal (scrollback survives) — with no drag on record for that tab (a dragged ratio holds
  until the terminals relaunch, per the rule above). Editing `split.cmd` or removing the block
  respawns the tab.
- A tab with no config split still ⌘D-splits on the right at 50/50, and that split survives
  a restart (the `localStorage` path is untouched).

Expect no look change to a live terminal: each surface's own render layer is already opaque
(`#0e1516`, `surface/ghostty.rs::new`), so it is never the pane ground a terminal composites
against. The always-on `--pane-ground` backstops only what no surface covers — the 1px focus
border ring, and a hole with no live surface in it (see the backstop footgun in `CLAUDE.md`).

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

**A popped-out left-side split lays out primary-left.** The detached shell lays holes out in
payload order and reports each hole by that index, and warden maps hole *i* to pane *i*, so
a `side = "left"` tab pops out mirrored. Unlock: a hole→pane index map on warden's
detached-window `set_hole_rect`/focus/close handlers (a warden-side mapping — shell-core's
generic ratio list needs no side concept). Not done now because pop-out is the rarer flow
and the mirrored layout is fully usable.
