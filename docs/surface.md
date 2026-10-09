---
type: reference
links:
  - rel: part-of
    to: CLAUDE.md
    note: CLAUDE.md points here before any work on the libghostty embed
---

# The terminal surface — the libghostty embed

Read before touching `crates/warden-app/src/surface/`, `ffi/`, `build.rs`'s header codegen, or
anything the native view and the web chrome both draw. Each entry names the rule and the code
site whose comment carries the rest.

The normal case: one `GhosttySurface` per pane, an `NSView` hosted on the window's content view
above the webview, sized to the rect the page reports for its hole (`reportRect` →
`set_hole_rect`, `geometry.rs`). libghostty's action stream (`surface/ghostty.rs::action_cb`)
drives links, notifications and child exit; everything else is AppKit plumbing around focus,
keys and rendering.

**Premise — surfaces spawn focused.** libghostty inits every surface `focus = 1`, which starts
its 60fps render display link; only *clearing* focus drops it to change-driven rendering. A
surface left wrongly focused burns 60fps and, while a background TUI redraws, flashes the text
cursor around an unfocused window.

## Vendoring and the FFI

`vendor/GhosttyKit.xcframework` is Ghostty built from a pinned, unmodified upstream commit by
[`lockyc/libghostty-build`](https://github.com/lockyc/libghostty-build) (CI-only; toolchain in that
repo and in [`PROVENANCE.md`](../crates/warden-app/vendor/PROVENANCE.md)), pulled
in by **`just revendor-ghostty`** (downloads the latest release, verifies its sha256, swaps
`vendor/`). The universal `libghostty.a` is committed, debug-stripped. The same release carries
`vendor/resources/`, which warden ships (terminfo, below). To move versions: bump `GHOSTTY_REF`
in libghostty-build → its CI republishes → `just revendor-ghostty` → update
[`vendor/PROVENANCE.md`](../crates/warden-app/vendor/PROVENANCE.md).

The embedding C API (`ghostty.h`) is upstream's **"libghostty-internal"**: tailored to the Ghostty
macOS app, mostly undocumented, "not designed for external use". Upstream points external embedders
at libghostty-vt, which has no renderer. warden stays on `ghostty.h` because the surface API is what
renders. On a version jump:
- **Struct layout** is guarded by the `const _` `size_of!` asserts in `ffi/mod.rs` — drift fails
  the build.
- **Action-tag discriminants are generated, never hand-copied.** `ghostty_action_tag_e` is a
  valueless C enum, so each tag is its position, and upstream inserts members mid-list. A drifted
  tag fails nothing: links go inert, `exit` waits for a keypress, a progress report unloads a live
  tab. `build.rs::generate_action_tags` derives every value from the header *inside* the
  xcframework (the one that ships with the linked `libghostty.a`) into `$OUT_DIR/action_tags.rs`.
  **Footgun:** never reintroduce a loose `vendor/ghostty.h` — `just revendor-ghostty` replaces the
  framework, not a header beside it, so a loose copy goes stale and regenerates the same drift.
  Add a tag by name to `ACTION_TAGS`.
- **Callback signatures aren't guarded.** The runtime callbacks are hand-transcribed
  function-pointer types, so a changed signature builds and links, then corrupts the call. Diff
  `ghostty_runtime_*_cb` and every `extern` fn warden declares against the new header.
- Eyeball any other enum transcribed positionally (`cursor_for_shape`).

## Links

Two link paths, both libghostty's: regex-matched URL text in the cells, and OSC 8 hyperlinks over
arbitrary label text. An `NSTrackingArea` forwards `mouseMoved`/`mouseExited` (link detection runs
on motion), `MOUSE_SHAPE` drives the cursor via cursor rects, `OPEN_URL` hands the link to
`NSWorkspace`. When a link misbehaves, check in this order:

1. **Under tmux, an OSC 8 link renders as inert styled text** — that is the tmux hop. tmux
   re-emits hyperlinks only when the client has the `hyperlinks` terminal feature, derived from
   terminfo `Hls`, which the bundled `xterm-ghostty` doesn't ship. Fix is per tmux socket:
   `set -as terminal-features 'xterm*:hyperlinks'` (agentmux sets it on its sockets; plain tmux
   needs it in `~/.tmux.conf`). Features are computed at client attach, so it lands on the next
   reattach. Diagnostic: `#{client_termfeatures}`.
2. **Under tmux `mouse on`, a link needs shift-⌘, to hover and to click.** Mouse reporting gates
   link-state refresh unless shift is held (`Surface.zig::cursorPosCallback`, `mouse-shift-capture`
   at its default `false`), and ⌘ is the link modifier. Plain ⌘ or plain shift does nothing. This
   is libghostty by design (Ghostty.app and iTerm match), so a shift-⌘-click that still fails means
   something upstream is broken — usually action-tag drift. Diagnostic: `tmux -L <socket> show -g mouse`.

## Child exit

Every exit, normal or abnormal, is first offered as the `SHOW_CHILD_EXITED` action; warden takes
it and unloads the tab to cold (a split's secondary exiting closes just that pane). Only if the
action is declined does libghostty print "Process exited. Press any key…" and, on that key, call
`close_surface_cb` — wired to the same signal as a fallback. `wait-after-command` is forced on
for any surface given a `command` (embedded.zig), so the fallback is never the primary path:
**"exit waits for a keypress" means the action isn't being matched (tag drift), never that warden
must wait.** Both paths reuse `Registry::unload`, and the chrome's shared `applyUnloaded` handles
`warden:tab-exited`.

**Never free a surface from inside `action_cb`** — libghostty is still standing on it, a silent
use-after-free that tests fine. `SHOW_CHILD_EXITED` defers via `dispatch_async_f`; any future
surface-destroying or lock-taking action takes the same path.

## Focus and keys

- **Seed a spawned surface's focus from the window's real key state** (`surface/ghostty.rs::new`,
  after `set_surface`). The key observers don't cover a never-key window — it emits neither.
- **`hide()` clears focus** — hiding a view doesn't stop its display link (`GhosttySurface::hide`).
- **libghostty focus tracks AppKit's first responder**: a `becomeFirstResponder` override (which
  surface within the window) plus per-window `windowDidBecomeKey:`/`windowDidResignKey:` observers
  (which window). Both are needed, or a clicked terminal types with a hollow cursor.
- **Window-scoped key traffic is claimed by ONE host view per window** — the first responder when
  it is a terminal view (`WardenHostView::owns_window_keys`, gating `performKeyEquivalent:` and
  `windowDidBecomeKey:`). AppKit routes function keys (arrows, F-keys, Home/End) through the
  key-equivalent pass to every host view in subview order, so a visibility-only gate let a split's
  primary steal arrows typed in the secondary.
- **The app menu gets first refusal** at the top of `performKeyEquivalent:`, or libghostty
  swallows the tab chords. `⌘\`` is reserved for macOS window cycling: return `NO`, never forward
  (a Kitty-protocol TUI would eat it).
- **Native focus is reported, never inferred by the chrome.** A click on live content is consumed
  by the `NSView`, so `becomeFirstResponder` raises `SurfaceSignal::Focused` — deferred a main-queue
  turn, since it also fires inside `activate`'s `focus()` under the `ManagerState` lock — and
  `manager.rs::handle_surface_focused` emits `warden:pane-focused`. The chrome's per-pane
  `mousedown` covers only a cold pane's backstop.
- **The pointer leaving the sidebar is reported natively too.** The surface takes the pointer the
  instant it leaves the chrome, so the page's `:hover` freezes after a fast flick. The host view's
  `mouseEntered:` raises `SurfaceSignal::PointerEntered` → `handle_pointer_entered` →
  `warden:pointer-away` → chrome-core's `pointerAway()`, emitted directly (AppKit delivers it from
  the event loop, never inside the lock). Any future hover-only chrome affordance inherits this.

## Rendering

- **Ship libghostty's terminfo** (`tauri.conf.json` `bundle.resources` +
  `main.rs::configure_ghostty_resources`, which clears an inherited `GHOSTTY_RESOURCES_DIR`;
  debug points at `vendor/resources/`). Without it every tab is `xterm-256color`, tmux loses the
  `Sync` cap, and redraws tear. Check: `echo $TERM` in a tab.
- **The render layer's background is opaque** (`#0e1516`, a properly-typed `CGColor`, in
  `surface/ghostty.rs::new`), or a cold tab flashes the wallpaper before first paint.
- **`contentsScale` tracks the backing scale** (`viewDidChangeBackingProperties`) — the layer
  scale is the lever, not the font path or a respawn. Multi-monitor refresh is separate:
  `ghostty_surface_set_display_id` via the `windowDidChangeScreen:` observer.
- **Corners: the layer rounds only the corners that are window corners**, concentric with the
  chrome's pane ring (`SURFACE_CORNER_RADIUS` + the continuous corner curve + `masksToBounds` +
  `apply_corner_mask`, re-run in `new`/`set_frame`/`reparent` from the frame touching the content
  view's edges — never plumbed from the page). The ring is a `::before` clipped by
  `clip-path: shape()` to Apple's three-Bézier continuous corner (`--sq`), not a `border-radius`,
  which can only curve as a circle and lifts off the window edge. `--ring-w` is 1px by choice; its
  softness round the curve on a 1x display is the accepted trade — don't thicken anything else to
  fix it. The radius lives on both sides of the seam (`--pane-radius` in `ui/index.html`,
  `--hole-radius` in shell-core's `detach.html`, `SURFACE_CORNER_RADIUS` = radius − ring) and
  changes together.
- **The surface frame is snapped inward to the device-pixel grid** (`geometry::snap_inward`, in
  `set_hole_rect`): ceil the origin, floor the far edge. Rounding to nearest or leaving it
  fractional lets the surface cover the ring's column along a divider at some window widths.
