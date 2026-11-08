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
for local use and take that count to one. The destination is zero: warden as the client of
Rex sessions (`docs/rex.md`), with tmux and agentmux retired (agentmux's `docs/multiplexer-direction.md`
maps where each agentmux capability goes).

What that buys beyond one less parse: native scrollback and selection in the second
surface, and a **per-client viewport**. Shared scroll across attached clients is tmux's,
not warden's, the moment the surface is warden's own.

## Native splits

A tab holds a **primary pane plus an optional scratch column**. On a plain tab the column is
one pane — two panes, not N-way; on a Rex tab it is a Rex session that stacks and tabs its own
panes (below). The config is the one source of a tab's split:

- **Declared in config** (`split` — cascading global → window → tab, and root → window →
  global for discovered projects; schema in `docs/config.md`). The tab is created with its
  second pane already declared, both spawn together (also under `load_on_open`), the
  second pane runs `split.cmd` (bare shell when absent) in the tab's own shell and dir, and
  `split.size`/`split.side` seed the divider. A drag overrides the ratio **in memory only**,
  for as long as the tab's terminals are live: unload, a child exit, a hot-reload respawn,
  a new `size` in the config and an app restart all bring the config ratio back; the
  right-click menu's Size ▸ saves a size, replacing the drag. `exit` in
  the second shell closes the pane for the same lifetime — `Registry::unload` re-declares it,
  so it is back on the next spawn — and ⌘D recreates it from config. This is the frame-less
  shape of an `amux --frame` tab: the scratch shell beside the agent, with warden owning the
  split instead of tmux.
- **⌘D (Tab ▸ Split) on a tab with no split saves one**: `split = true` onto the tab's own
  config entry (a `[[window.root.tab]]` override for a discovered project —
  `split_write::save_split`), and the hot-reload brings the second pane up beside the running
  primary (`reconcile`'s `split_tabs` → `Registry::split_from_config`); removing the split
  from the config closes its pane (`unsplit_tabs`) — on a Rex tab the column's session only
  detaches, and adding the split back reattaches it. Neither touches the primary's PTY. While the
  config doesn't parse, the save is refused with a banner.

A split is closed by the column's panes ending — `exit` in one, ⌘W with focus in it, or the
right-click Close Pane, which lands as the exit does — or by the config dropping it, and resized
by dragging the divider, a 1px line in a 5px grab strip that carries no close control. ⌘W
follows focus (`Registry::close_target`): in the main pane it unloads the **whole tab** (or
closes a selected agent tab), and ⌘⇧O (pop-out) acts on the whole tab too — unloading drops
every pane to cold together, and popping out carries both surfaces
to the detached window at the tab's current effective ratio
(`shell_core::detach::DetachSpec.panes`, generic on purpose: a `Vec<f64>` of hole ratios,
not a `warden` split type, so a future consumer can divide its own hole without learning
warden's split concept).

**On a Rex tab the column is one Rex session** — the split pane's (`rex/column.rs`). Each of
its Rex windows is a **scratch tab** in the `#scratch-tabs` strip (`[1×] [2] [+]`) at the
column's top; each window's split tree (`session.view`'s `layers[0].layout`, read by
`rex/layout.rs`) is the blocks stacked in it, one surface + relay per block. Rex owns the
windows and layout (the `docs/rex.md` invariant), so a split made in Rex.app shows here and
vice versa. This replaces agentmux's `[frame]` `left_vertical_split` / `left_top_panes`:

- **Config declares the first scratch tab's blocks** as a vertical stack (`[[…split.pane]]`,
  schema in `docs/config.md`); warden creates the session in that layout
  (`layout::stack_layout`), and after that Rex holds it, so a stack edit reaches only sessions
  created afterwards. No stack (`split = true`, what ⌘D saves) is one bare shell.
- **`+`** or Tab ▸ New Scratch Tab (⌃⌘T) opens a new Rex window with a bare shell; clicking a
  number shows that scratch tab and leaves the main pane alone; the shown one's **×** closes it
  and its blocks (`session.close_window`).
- **⌘D in a block** is `session.new_split` below it, and focus moves to the new block when
  Rex's next view brings it (`Registry::focus_block`); dragging an inner divider is
  `session.resize_pane`, sent on release.
- **`exit` in a block**, or ⌘W / Close Pane on it (`block.close`), closes it: its scratch tab
  reflows, or goes with its last block and the selection leans to a neighbour; the session's
  last block ends the column and collapses the split (`docs/rex.md`, *Lifecycle*). While the
  column's open is in flight, ⌘W in it is disabled with the reason.
- **Pop-out carries at most one block** — a fuller column is refused, with Tab ▸ Pop Out Tab
  disabled and titled with the reason (`Registry::pop_out_blocked`; the unlock is in
  `docs/FOLLOWUPS.md`).

Plain tabs keep the one-pane column: a stack there needs a warden-owned layout model; unlock: a
plain-tab workflow that needs more than two panes.

Mechanism: `warden-config`'s `Tab.split: Option<Split>` (`resolve.rs::resolve_split_level` +
`cascade_split`; `reconcile.rs::reconcile` lists which edits respawn and which ride
`set_meta`), `Registry`'s `Pane`/`PaneIdx`/`Column`/`TabSlot` model
(`crates/warden-app/src/registry.rs` — `add` declares a config split's column, one
`secondary_spec` derives its startup, `sync_column` follows a Rex column's session),
`TabDto.split_layout` and `TabDto.column` (`ColumnDto`) → the chrome's
`splitLayoutById`/`ratioOverride` and `scratchRender`, the `split_pane`/`focus_pane` and
`scratch_select`/`scratch_new`/`scratch_resize` commands (`main.rs`), and the chrome's
pane/divider DOM + `setSplitVisible` (`crates/warden-app/ui/index.html`).
Read *Traps* below before touching split visibility, routing, or the backstop.

## Pop-out: session-preserving, whole-tab

A ⤢ on the tab row (chrome-core's `onPopOut`) or **⌘⇧O** pops the active tab into shell-core's
banner-only detach window. **Session-preserving is the defining property**:
`GhosttySurface::reparent` moves the live `NSView` onto the new window (re-registers its three
window-scoped observers, swaps the `window` field, re-seeds display id + focus, reapplies
geometry) and calls none of `teardown`/`ghostty_surface_free`/`close` — PTY, scrollback and
process survive both trips. `Registry::detach` takes the `Spawned` surface of every pane and
leaves `TabSlot::Detached` in each slot; closing the detached window returns the tab via
`manager.rs::redock` → `Registry::attach` (`Cold`/`Detached` → `Spawned`, both panes or neither),
reopening the origin window first if it was closed. The origin row shows muted via the forwarded
`detached` DTO field and clicking it calls `raise_popped_window`. The detached window's geometry
persists under its `shell-detach:` label, derived by `plan::detach_window_token` from
`origin_label:tab_key`.

## Traps

- **Pop-out backend invariants** (`main.rs::pop_out_tab`, `manager.rs::redock`,
  `registry.rs::detach`/`attach`): extract the surfaces under the `ManagerState` lock → build the
  window + `reparent` with the lock **released** → re-lock to store and wire the return. The
  `Detached` placeholder — `detach`'s, or `mark_detached`'s in an origin rebuilt while the tab is
  out — keeps `reconcile` from touching the tab; `redock` reconciles it against the config once home
  (`reconcile_returned_tab`); `detached` is counted into the `WindowSet` (`sync_detached`); `is_quitting` stops `redock`
  resurrecting mid-teardown. **Every failure path restores both panes**
  — a partial restore strands a live PTY with no window and no route back.
- **Size the reparented surface's birth rect from the built window** (the `size` `open_detached` hands
  `pop_out_tab`'s birth closure), never `DETACHED_DEFAULT_*` — the geometry plugin restores the
  remembered size during `build()`.
- **A detached window's `pane` index is a hole (left-to-right), never a `PaneIdx`.** Resolve with
  `PaneIdx::from_hole(i, ds.mirrored())`, emit with `which.hole(ds.mirrored())`. The docked chrome
  draws a `side = "left"` split with a flex `order`, so the detached layout mirrors on warden's
  side; `mirrored` keys on the secondary's *presence*. `registry.rs::PaneIdx::hole`,
  `manager.rs::DetachedSurface::mirrored`.
- **Surface→tab routing searches every pane and `detached` too**
  (`Registry::locate_surface`, `manager.rs::locate_detached_surface`, `handle_child_exited`'s
  detached arms). Missing either, a split tab silently stops badging or a dead shell sits under
  "Process exited".
- **Split visibility is the `.split` class on `#terminal-hole`, never `hidden` alone** — `hidden`'s
  UA `display: none` loses to any author `display` rule, so `#pane-divider`/`#pane-secondary`'s
  rule is scoped to `#terminal-hole.split`. **`applySplitShape` is the only writer** of `.split`
  and `hidden` — `setSplitVisible` for the active tab; `measureTabs` borrows it per hidden tab and
  restores the saved state in the same task (keep that restore synchronous).
- **A split's ratio never reaches `localStorage`** — a drag lands in the in-memory
  `ratioOverride`, cleared wherever the tab goes cold or its declared `size` changes.
- **Report the pane's content box, and paint the border box's own background** — both, together:
  the border box lets the surface occlude the focus border; the content box without an opaque
  outer ground leaks wallpaper round every pane. `ui/index.html` (`contentRect` + the `.pane` CSS).
- **The per-pane backstop ground is always on; only the illustration toggles**
  (`#empty-state`, each block's `.scratch-empty`). Behind a live surface it is invisible, so an uncovered hole is
  unrepresentable on every transient.
- **Every `localStorage` access is wrapped whole, `.length` and `.key()` included** —
  unavailable storage throws on every member, and an unguarded read stops the chrome's script.

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
  the origin forgets the dragged ratio).
- A popped-out tab whose primary shell exits comes home cold: the detached window closes,
  the origin row goes hollow (click to respawn), and the sidebar leans to a live neighbour
  exactly as a docked exit does.
- Popping out an unsplit tab is unchanged — one hole, no `panes` in the detach payload.
- Popping out a split tab carries both panes into the detached window at the same ratio,
  with the same divider and focus ring; `exit` in the second pane closes it there too, and
  clicking either pane moves the ring.
- Popping out a `side = "left"` split keeps the second pane on the LEFT (same sides as
  docked — `PaneIdx::hole`); dragging the divider there resizes the pane under the cursor,
  and `exit` in the left pane collapses the window to the primary alone.
- Closing the detached window returns both panes to the origin, still live.
- Arrow keys pressed in the second pane stay in it: the cursor, the focus ring and the
  keystroke all remain on the pane you typed in (arrows travel a different AppKit route
  from letters — `WardenHostView::owns_window_keys` in `docs/surface.md`).
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
  terminal (scrollback survives), replacing any dragged ratio. Editing `split.cmd` respawns
  the tab; removing the block closes the second pane and leaves the primary running.
- ⌘D on a tab with no split writes `split = true` onto its entry in the config file, and the
  second pane opens once, on the right at 50/50, while the primary keeps its scrollback; the
  split survives a restart. With the config broken mid-edit, ⌘D shows a banner and nothing
  splits.
- With a local Rex running, `just run`'s `stack` tab opens its column stacked at the declared
  sizes; `+` adds scratch tab 2 with one shell, and clicking `1`/`2` swaps the column while the
  agent pane stays put; ⌘D in a block adds a block below it, focused; dragging an inner divider
  resizes only its two sides and survives a tab switch; `exit` in a non-last block reflows the
  rest; `exit` in tab 2's only block returns to tab 1, and ⌘W and the shown tab's × do the
  same as `exit`, for a block and a whole tab; a split made in Rex.app on the same
  session appears in warden; Tab ▸ Pop Out Tab on the stacked tab is disabled with its reason.

Expect no look change to a live terminal: each surface's own render layer is already opaque
(`#0e1516`, `surface/ghostty.rs::new`), so it is never the pane ground a terminal composites
against. The always-on `--pane-ground` backstops only what no surface covers — the `--ring-w` focus
border ring, and a hole with no live surface in it (see the backstop trap above).

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

**A popped-out left-side split's focus ring starts on the wrong hole.** shell-core's detach
page focuses its FIRST hole on every layout, which for a mirrored (`side = "left"`) pop-out
is the secondary; the ring is right the moment either pane is clicked (`handle_surface_focused`
→ `set_focused_hole`, by hole). Unlock: an initial-focus hole index in `DetachSpec`/the detach
payload (a layout fact, generic enough for shell-core) — an emit from warden right after
`open_detached` returns can race the page's own listener registration and be lost, so the
payload is the reliable route. Not done now because it is one click's worth of wrongness.
