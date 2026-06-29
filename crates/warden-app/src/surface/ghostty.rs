//! The libghostty implementation of `TerminalSurface`.
//!
//! This is the only place (besides `ffi`) that talks to libghostty or AppKit.
//! It solves the three hard sub-problems of Checkpoint 1:
//!
//! 1. **Runtime config + callbacks** — the full `ghostty_runtime_config_s` is
//!    built in `create_app()` with a real `wakeup_cb`; the rest are no-ops
//!    sufficient for a single static surface (see each fn for why).
//! 2. **Runloop ownership** — Tauri owns the `NSApplication` + main runloop.
//!    We do NOT start a second one. `wakeup_cb` (called from any thread) just
//!    `dispatch_async`-es a `ghostty_app_tick` onto the main GCD queue, which
//!    the Tauri-owned runloop drains. This mirrors Ghostty's own macOS app
//!    (`Ghostty.App.swift`: `wakeup` -> `DispatchQueue.main.async { appTick() }`).
//! 3. **View insertion + focus** — we create a custom `NSView` subclass
//!    (`WardenHostView`) so we can forward `keyDown:`/`keyUp:` into
//!    `ghostty_surface_key` (libghostty does NOT capture the keyboard itself;
//!    the host app must forward events — Ghostty's `SurfaceView` does the same).
//!    The host view is added as the topmost subview of the window content view
//!    (above the WKWebView) and made first responder.
//!
//! Threading: `GhosttySurface` holds retained AppKit objects and a raw surface
//! pointer, so it is not auto-`Send`. We `unsafe impl Send` ONLY so it can live
//! in Tauri-managed state; every method must be called on the main/UI thread
//! (Tauri's `setup` and command handlers run there). This is a spike-scoped
//! affordance, documented here as the single load-bearing invariant.

use super::{PixelRect, SurfaceError, SurfaceEvent, SurfaceSignal, TabSpec, TerminalSurface};
use crate::ffi;
use crate::geometry;

use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::OnceLock;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{
    class, declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_app_kit::{
    NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSEvent,
    NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF, NSResponder,
    NSView, NSWindow, NSWindowDidBecomeKeyNotification, NSWindowDidResignKeyNotification,
};
use objc2_foundation::{
    MainThreadMarker, NSData, NSDictionary, NSNotification, NSNotificationCenter, NSPoint, NSRect,
    NSSize, NSString,
};

// --- AppKit modifier-flag bit masks (stable AppKit ABI values) ---
const NS_FLAG_CAPS: usize = 1 << 16;
const NS_FLAG_SHIFT: usize = 1 << 17;
const NS_FLAG_CONTROL: usize = 1 << 18;
const NS_FLAG_OPTION: usize = 1 << 19;
const NS_FLAG_COMMAND: usize = 1 << 20;

// --- Process-global state ---------------------------------------------------
// The shared ghostty app handle (created once). Stored as usize so the static
// is trivially Send/Sync; reconstituted to a pointer on read.
static GHOSTTY_APP: OnceLock<usize> = OnceLock::new();

// --- libdispatch: hop a ghostty_app_tick onto the main GCD queue ------------
// dispatch_get_main_queue() is a static-inline in C that returns &_dispatch_main_q;
// we reference that global symbol directly (exported by libSystem).
#[repr(C)]
struct DispatchObjectS {
    _private: [u8; 0],
}
extern "C" {
    static _dispatch_main_q: DispatchObjectS;
    fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}
fn main_queue() -> *mut c_void {
    ptr::addr_of!(_dispatch_main_q) as *mut c_void
}
/// GCD work item: runs on the main thread, ticks the app passed as `context`.
unsafe extern "C" fn tick_trampoline(context: *mut c_void) {
    if !context.is_null() {
        ffi::ghostty_app_tick(context as ffi::ghostty_app_t);
    }
}

// --- Runtime callbacks ------------------------------------------------------
/// Called by libghostty (from any thread) when it has main-thread work pending.
/// We coalesce nothing; just schedule a tick on the Tauri-owned main runloop.
unsafe extern "C" fn wakeup_cb(_userdata: *mut c_void) {
    let app = GHOSTTY_APP.get().copied().unwrap_or(0) as *mut c_void;
    if app.is_null() {
        return;
    }
    dispatch_async_f(main_queue(), app, tick_trampoline);
}

/// App/surface actions (set-title, new-window, ring-bell, desktop-notification, ...). warden acts
/// on the two attention signals — `RING_BELL` and `DESKTOP_NOTIFICATION` — decoding them into a
/// seam-neutral `SurfaceEvent` and forwarding to the app-level sink (which routes to the owning
/// tab). All other actions are unhandled; returning false = "not handled", which the reference
/// (`Ghostty.App.swift`) also does for every unimplemented action. Runs on the main thread (called
/// from a `ghostty_app_tick`).
unsafe extern "C" fn action_cb(
    _app: ffi::ghostty_app_t,
    target: ffi::ghostty_target_s,
    action: ffi::ghostty_action_s,
) -> bool {
    // Only surface-targeted signals map to a tab; app-level targets have nowhere to route.
    let Some(surface) = target.surface() else {
        return false;
    };
    let signal = if action.is_ring_bell() {
        Some(SurfaceSignal::Bell)
    } else if let Some(dn) = action.desktop_notification() {
        // Copy the borrowed C strings out now — libghostty frees them when this call returns.
        let read = |p: *const c_char| {
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        Some(SurfaceSignal::Notification {
            title: read(dn.title),
            body: read(dn.body),
        })
    } else {
        None
    };
    match signal {
        Some(signal) => {
            super::emit_surface_event(SurfaceEvent {
                surface_id: surface as usize,
                signal,
            });
            true
        }
        None => false,
    }
}

/// The surface that should receive clipboard reads (paste). libghostty's `read_clipboard_cb` is
/// app-level — it carries no surface — so we track the currently-focused surface here and answer
/// the request against it. This is the one piece of process-global surface state (keys route via
/// the per-view ivar instead); it exists only because the clipboard callback has no surface to
/// hang context on. Written by `focus()` (tab activate) and refreshed in `performKeyEquivalent:`
/// (so focus that arrives by clicking another window — never through activate — still retargets
/// paste). Cleared on `close()` so a freed surface is never completed against (UAF).
static FOCUSED_SURFACE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Monotonic counter for temp image-paste filenames (see `clipboard_image_to_temp_path`).
static PASTE_IMAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Paste: libghostty asks for clipboard data (e.g. on ⌘V); read the macOS general pasteboard and
/// hand it back via `complete_clipboard_request`. Runs on the main thread (from a `ghostty_app_tick`).
///
/// Text wins. If the clipboard is image-only (a screenshot, a copied image — no text), spill it to
/// a temp PNG and paste that *path* instead — Claude Code (and friends) pick up a pasted image off
/// bracketed paste exactly the way drag-and-drop delivers a file path. The image bytes never transit
/// the PTY; the consuming program reads the file itself. This is what makes ⌘V-a-screenshot work.
unsafe extern "C" fn read_clipboard_cb(
    _userdata: *mut c_void,
    _loc: ffi::ghostty_clipboard_e,
    state: *mut c_void,
) -> bool {
    let surface = FOCUSED_SURFACE.load(Ordering::Acquire);
    if surface.is_null() {
        return false;
    }
    let pb = NSPasteboard::generalPasteboard();
    let payload = pb
        .stringForType(NSPasteboardTypeString)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| clipboard_image_to_temp_path(&pb))
        .unwrap_or_default();
    let Ok(c_text) = CString::new(payload) else {
        return false; // clipboard contained an interior NUL — refuse rather than truncate
    };
    ffi::ghostty_surface_complete_clipboard_request(surface, c_text.as_ptr(), state, true);
    true
}

/// If the general pasteboard carries raster image data (and no text), write it to a temp PNG and
/// return that path; `None` when there's no image. PNG is emitted unconditionally — consumers match
/// on image *extensions*, and macOS screenshots land on the clipboard as TIFF, so a TIFF-only
/// clipboard is transcoded first. Files accrete in the temp dir for the session; the OS reaps them.
unsafe fn clipboard_image_to_temp_path(pb: &NSPasteboard) -> Option<String> {
    let png: Retained<NSData> = match pb.dataForType(NSPasteboardTypePNG) {
        Some(d) => d,
        None => {
            let tiff = pb.dataForType(NSPasteboardTypeTIFF)?;
            let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
            let props: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> =
                NSDictionary::new();
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props)?
        }
    };
    // Create the temp file O_EXCL with 0600 perms rather than `fs::write` (which follows symlinks
    // and truncates whatever's at the path, and leaves the bytes umask-default 0644). This only
    // matters if $TMPDIR is world-writable — default macOS $TMPDIR is the per-user 0700
    // `/var/folders/…/T`, so there's no live exposure — but it's the correct way to land another
    // process's screenshot bytes. pid+seq can collide with a stale file from a prior run that
    // reused this pid (files accrete for the session), so retry on AlreadyExists.
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let pid = std::process::id();
    for _ in 0..64 {
        let n = PASTE_IMAGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("warden-paste-{}-{}.png", pid, n));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(png.bytes()).ok()?;
                return Some(path.to_str()?.to_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}
unsafe extern "C" fn confirm_read_clipboard_cb(
    _userdata: *mut c_void,
    _str: *const c_char,
    _state: *mut c_void,
    _request: ffi::ghostty_clipboard_request_e,
) {
}
/// Copy: libghostty hands us the selected text (e.g. on ⌘C or copy-on-select); write it to the
/// macOS general pasteboard. macOS has no primary-selection clipboard, so we ignore SELECTION
/// writes. Runs on the main thread (called from a `ghostty_app_tick`).
unsafe extern "C" fn write_clipboard_cb(
    _userdata: *mut c_void,
    loc: ffi::ghostty_clipboard_e,
    content: *const ffi::ghostty_clipboard_content_s,
    len: usize,
    _confirm: bool,
) {
    if loc != ffi::ghostty_clipboard_e::GHOSTTY_CLIPBOARD_STANDARD || content.is_null() || len == 0
    {
        return;
    }
    // Take the first entry that carries valid UTF-8 text (usually the text/plain mime).
    let entries = std::slice::from_raw_parts(content, len);
    let Some(text) = entries.iter().find_map(|e| {
        (!e.data.is_null())
            .then(|| std::ffi::CStr::from_ptr(e.data).to_str().ok())
            .flatten()
    }) else {
        return;
    };
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
}
/// Surface requested close (e.g. shell exited). The spike keeps the window;
/// teardown is `GhosttySurface::close`. No-op here.
unsafe extern "C" fn close_surface_cb(_userdata: *mut c_void, _process_alive: bool) {}

// --- Shared app -------------------------------------------------------------
/// Build the ghostty app exactly once. Returns 0 on failure.
unsafe fn create_app() -> usize {
    let config = ffi::ghostty_config_new();
    if config.is_null() {
        return 0;
    }
    // Load the user's ghostty config (if any) then finalize, matching the
    // reference's Config(at:) sequence. ghostty_app_new takes ownership of
    // the config, so we deliberately do not free it.
    ffi::ghostty_config_load_default_files(config);
    ffi::ghostty_config_finalize(config);

    let runtime = ffi::ghostty_runtime_config_s {
        userdata: ptr::null_mut(),
        supports_selection_clipboard: false,
        wakeup_cb: Some(wakeup_cb),
        action_cb: Some(action_cb),
        read_clipboard_cb: Some(read_clipboard_cb),
        confirm_read_clipboard_cb: Some(confirm_read_clipboard_cb),
        write_clipboard_cb: Some(write_clipboard_cb),
        close_surface_cb: Some(close_surface_cb),
    };

    let app = ffi::ghostty_app_new(&runtime, config);
    app as usize
}
fn shared_app() -> ffi::ghostty_app_t {
    (*GHOSTTY_APP.get_or_init(|| unsafe { create_app() })) as ffi::ghostty_app_t
}

// --- Custom NSView that forwards keyboard events to libghostty --------------
declare_class!(
    struct WardenHostView;

    unsafe impl ClassType for WardenHostView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WardenHostView";
    }

    impl DeclaredClass for WardenHostView {
        type Ivars = HostIvars;
    }

    unsafe impl WardenHostView {
        #[method_id(initWithFrame:)]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> Option<Retained<Self>> {
            let this = this.set_ivars(HostIvars {
                surface: Cell::new(ptr::null_mut()),
                built_scale: Cell::new(0.0),
            });
            unsafe { msg_send_id![super(this), initWithFrame: frame] }
        }

        // Must be true for the view to accept key events as first responder.
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // AppKit makes a clicked terminal view first responder directly — bypassing focus() (tab
        // activate) and performKeyEquivalent: (⌘-chords), the only two paths that ever told
        // libghostty about focus. So a plain mouse-click into a surface routed keystrokes here
        // (keyDown: → this view's surface) while libghostty still believed the surface unfocused
        // (some other surface having claimed focus via a chord, which set this one false) — and it
        // drew a HOLLOW cursor that nonetheless accepted typing (the "cursor isn't filled but still
        // types" bug). Sync the focus flag here: becomeFirstResponder: is the one AppKit hook every
        // focus change funnels through (click, makeFirstResponder, key-window restore), so the
        // cursor state now tracks the real first responder. Also retarget paste (FOCUSED_SURFACE) so
        // a click makes this the clipboard-read surface, consistent with performKeyEquivalent:.
        #[method(becomeFirstResponder)]
        fn become_first_responder(&self) -> bool {
            let surface = self.ivars().surface.get();
            if !surface.is_null() {
                FOCUSED_SURFACE.store(surface, Ordering::Release);
                unsafe { ffi::ghostty_surface_set_focus(surface, true) };
            }
            true
        }

        // becomeFirstResponder: only fires on a *change* of first responder — AppKit doesn't re-send
        // it when a window merely re-becomes key with its old first responder intact. So switching
        // apps/windows away and back left libghostty's focus flag stale: a surface stayed focused
        // (filled cursor) in a now-non-key window, or stayed hollow after the window came back. These
        // two observers (registered per-window in `new`, scoped via `object: window`) track key state
        // directly. Gate on `!isHidden()` so only the active tab's view in the keyed window reacts —
        // every tab view in a window receives the notification, but exactly one is unhidden (show()/
        // hide()), the same disambiguation performKeyEquivalent: uses.
        #[method(windowDidBecomeKey:)]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            let surface = self.ivars().surface.get();
            // SAFETY: notifications are delivered on the main thread.
            if surface.is_null() || unsafe { self.isHidden() } {
                return;
            }
            FOCUSED_SURFACE.store(surface, Ordering::Release);
            unsafe { ffi::ghostty_surface_set_focus(surface, true) };
        }

        #[method(windowDidResignKey:)]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            let surface = self.ivars().surface.get();
            // SAFETY: notifications are delivered on the main thread.
            if surface.is_null() || unsafe { self.isHidden() } {
                return;
            }
            unsafe { ffi::ghostty_surface_set_focus(surface, false) };
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            unsafe { forward_key(self, event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS) };
        }

        #[method(keyUp:)]
        fn key_up(&self, event: &NSEvent) {
            unsafe { forward_key(self, event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_RELEASE) };
        }

        // ⌘-combo key-DOWN events arrive via performKeyEquivalent:, NOT keyDown:. Two owners can
        // want them: the app menu (tab nav ⌘⇧[/⌘⇧], ⌘1–9, ⌘Q/⌘W/…) and libghostty's own keybinds.
        // macOS consults the view hierarchy BEFORE the main menu, and libghostty binds the very
        // same standard tab chords — so forwarding first let it swallow them (consumed=true) and
        // the menu never fired (the "inconsistent tab switch" bug). Give the main menu first
        // refusal: if it owns the chord, let it act and stop. Otherwise forward to libghostty
        // (paste/copy/…) and return whether it consumed — if not, return NO so AppKit can still
        // route ⌘` and friends. The menu's accelerators define the reserved set (self-maintaining).
        #[method(performKeyEquivalent:)]
        fn perform_key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            if let Some(mtm) = MainThreadMarker::new() {
                let main_menu = unsafe { NSApplication::sharedApplication(mtm).mainMenu() };
                if let Some(menu) = main_menu {
                    if unsafe { menu.performKeyEquivalent(event) } {
                        return objc2::runtime::Bool::YES;
                    }
                }
            }

            let surface = self.ivars().surface.get();
            // Disambiguate sibling tab views: the hidden views in this window ALSO receive
            // performKeyEquivalent:, but exactly one host view per window is unhidden — the
            // active tab (show()/hide() enforce this). Gate on visibility, NOT a match against
            // FOCUSED_SURFACE: only the key window's hierarchy receives this event, so the lone
            // unhidden view here is the focused surface globally — even when focus arrived by a
            // click that never ran activate() (clicking another window's terminal or sidebar).
            // Keying the gate on activate()-set state was the bug: ⌘C/⌘V went dead in a window
            // focused by click until a tab row was clicked.
            // SAFETY: performKeyEquivalent: is delivered on the main thread.
            if surface.is_null() || unsafe { self.isHidden() } {
                return objc2::runtime::Bool::NO;
            }
            // This chord proves THIS surface is the focused one. Make it the global paste/focus
            // target so the ⌘V being processed now lands here (libghostty's read_clipboard_cb is
            // app-level and reads FOCUSED_SURFACE), and hand libghostty focus over from whatever
            // surface held it — only on a real change, to avoid resetting cursor blink each press.
            let prev = FOCUSED_SURFACE.swap(surface, Ordering::AcqRel);
            if prev != surface {
                unsafe {
                    if !prev.is_null() {
                        ffi::ghostty_surface_set_focus(prev, false);
                    }
                    ffi::ghostty_surface_set_focus(surface, true);
                }
            }
            let consumed =
                unsafe { forward_key(self, event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS) };
            objc2::runtime::Bool::new(consumed)
        }

        // --- Mouse: forward button/drag/scroll so terminal mouse modes (tmux pane select,
        // scrollback, TUI clicks) work. `mouseMoved` (hover with no button) needs an
        // NSTrackingArea and is deferred — click/drag/scroll cover the core interactions. ---
        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_LEFT) };
        }
        #[method(mouseUp:)]
        fn mouse_up(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_LEFT) };
        }
        #[method(mouseDragged:)]
        fn mouse_dragged(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }
        #[method(rightMouseDown:)]
        fn right_mouse_down(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_RIGHT) };
        }
        #[method(rightMouseUp:)]
        fn right_mouse_up(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_RIGHT) };
        }
        #[method(rightMouseDragged:)]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }
        #[method(otherMouseDown:)]
        fn other_mouse_down(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_MIDDLE) };
        }
        #[method(otherMouseUp:)]
        fn other_mouse_up(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_MIDDLE) };
        }
        #[method(otherMouseDragged:)]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }
        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            unsafe { forward_scroll(self, event) };
        }

        // Fired by AppKit when the view's backing properties change — specifically when the window
        // moves to a screen with a *different* backingScaleFactor (laptop Retina 2× ↔ external 1×).
        // Re-push content scale + size so libghostty's cell metrics track the new DPI; without this
        // the surface keeps the creation-time scale and the terminal stops filling the window (and
        // a later resize scales the grid proportionally instead of reflowing). A plain window *move*
        // between same-scale displays doesn't fire this, which is correct — nothing to update.
        #[method(viewDidChangeBackingProperties)]
        fn view_did_change_backing_properties(&self) {
            let surface = self.ivars().surface.get();
            if surface.is_null() {
                return;
            }
            let Some(window) = self.window() else { return };
            let scale = window.backingScaleFactor();

            // THE fix for "terminal is scaled wrong after unplugging a monitor". libghostty's macOS
            // renderer makes this view layer-hosting — it assigns its own render layer to `view.layer`
            // and sets that layer's `contentsScale` *only at creation* (renderer/Metal.zig); its live
            // content-scale path never updates it. So when the window moves to a different-DPI display,
            // Core Animation keeps compositing the layer at the stale scale and magnifies/shrinks the
            // whole surface. Push the new scale onto the layer ourselves — exactly as Ghostty.app's
            // own SurfaceView does — inside a CATransaction with actions disabled so CA doesn't animate
            // the change. The font/cell metrics themselves reflow via the `set_content_scale` below;
            // this line is purely the compositor's scale. SAFETY: main-thread AppKit; `layer` is this
            // view's CALayer (or null before it's layer-hosting), only messaged, never retained.
            unsafe {
                let layer: *mut AnyObject = msg_send![self, layer];
                if !layer.is_null() {
                    let _: () = msg_send![class!(CATransaction), begin];
                    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
                    let _: () = msg_send![layer, setContentsScale: scale];
                    let _: () = msg_send![class!(CATransaction), commit];
                }
            }

            let frame = self.frame();
            unsafe {
                apply_surface_geometry(surface, frame.size.width, frame.size.height, scale);
            }

            // Opt-in fallback (`respawn_on_scale_change`, default off): if the contentsScale fix above
            // ever proves insufficient on some display, recreating the surface always renders correctly
            // (creation rebuilds everything from the current scale). Signal it async so the teardown
            // doesn't run while this NSView method is on the stack; the manager drops the signal when
            // the flag is off. Gated on a real change (live scale ≠ the view's creation scale).
            if (scale - self.ivars().built_scale.get()).abs() > f64::EPSILON {
                unsafe {
                    dispatch_async_f(main_queue(), surface as *mut c_void, scale_changed_trampoline);
                }
            }
        }
    }
);

/// Per-view state: the surface this view forwards keystrokes to. Set once, right
/// after the surface is created in `GhosttySurface::new`. Holding it per-view
/// (rather than in a process global) makes multi-window key routing correct by
/// construction — AppKit first-responder routing delivers `keyDown:` to the
/// focused window's view, which forwards to *its own* surface.
struct HostIvars {
    surface: Cell<ffi::ghostty_surface_t>,
    /// The backing scale the surface's font was built at (creation-time `backingScaleFactor`).
    /// `viewDidChangeBackingProperties` compares the live scale against this to detect a *real*
    /// DPI change (the override also fires on no-op backing changes / at launch), and signals a
    /// respawn only when they differ. Set once in `new`; a respawn makes a fresh view with the
    /// new scale, so it never needs updating in place.
    built_scale: Cell<f64>,
}

impl WardenHostView {
    fn set_surface(&self, surface: ffi::ghostty_surface_t) {
        self.ivars().surface.set(surface);
    }
    fn set_built_scale(&self, scale: f64) {
        self.ivars().built_scale.set(scale);
    }
}

/// True when `s` is genuinely-typed printable text — the only case we forward as the key
/// event's `text` to libghostty. macOS `characters` for a Ctrl/Cmd combo is the C0 control
/// char (Ctrl-F → "\u{06}"); handing *that* to libghostty as `text` makes it emit a Kitty
/// `CSI…u` sequence (e.g. `^[[6;5u`) instead of the bare control byte (`^F`) that legacy apps —
/// tmux, less, vim — read as the keypress. Ghostty's own macOS app omits `text` for these and
/// lets the key encoder derive the byte from key + mods.
fn is_printable_text(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_control())
}

/// Map an AppKit event's modifier flags to ghostty's mods bitset. Shared by the key and
/// mouse paths so both report the same modifier state.
unsafe fn mods_from_event(event: &NSEvent) -> ffi::ghostty_input_mods_e {
    let flags = event.modifierFlags().0;
    let mut mods = ffi::GHOSTTY_MODS_NONE;
    if flags & NS_FLAG_SHIFT != 0 {
        mods |= ffi::GHOSTTY_MODS_SHIFT;
    }
    if flags & NS_FLAG_CONTROL != 0 {
        mods |= ffi::GHOSTTY_MODS_CTRL;
    }
    if flags & NS_FLAG_OPTION != 0 {
        mods |= ffi::GHOSTTY_MODS_ALT;
    }
    if flags & NS_FLAG_COMMAND != 0 {
        mods |= ffi::GHOSTTY_MODS_SUPER;
    }
    if flags & NS_FLAG_CAPS != 0 {
        mods |= ffi::GHOSTTY_MODS_CAPS;
    }
    mods
}

/// Forward the cursor position to libghostty. AppKit gives window coordinates with a
/// bottom-left origin; ghostty wants the surface's top-left origin, so convert into the
/// view's space and flip Y. Called before every button/scroll so the surface has the cell.
unsafe fn forward_mouse_pos(view: &WardenHostView, event: &NSEvent) {
    let surface = view.ivars().surface.get();
    if surface.is_null() {
        return;
    }
    let local = view.convertPoint_fromView(event.locationInWindow(), None);
    let height = view.bounds().size.height;
    ffi::ghostty_surface_mouse_pos(surface, local.x, height - local.y, mods_from_event(event));
}

/// Forward a mouse button press/release (position is updated first so the click lands on the
/// right cell).
unsafe fn forward_mouse_button(
    view: &WardenHostView,
    event: &NSEvent,
    state: ffi::ghostty_input_mouse_state_e,
    button: ffi::ghostty_input_mouse_button_e,
) {
    let surface = view.ivars().surface.get();
    if surface.is_null() {
        return;
    }
    forward_mouse_pos(view, event);
    ffi::ghostty_surface_mouse_button(surface, state, button, mods_from_event(event));
}

/// Forward a scroll/trackpad event. The precision bit tells libghostty whether the deltas are
/// pixel-precise (trackpad) or line-based (classic wheel).
unsafe fn forward_scroll(view: &WardenHostView, event: &NSEvent) {
    let surface = view.ivars().surface.get();
    if surface.is_null() {
        return;
    }
    forward_mouse_pos(view, event);
    let mods: ffi::ghostty_input_scroll_mods_t = if event.hasPreciseScrollingDeltas() {
        1
    } else {
        0
    };
    ffi::ghostty_surface_mouse_scroll(
        surface,
        event.scrollingDeltaX(),
        event.scrollingDeltaY(),
        mods,
    );
}

/// Translate an AppKit key event into `ghostty_input_key_s` and forward it.
/// Minimal translation: `text` (from `characters`) carries printable input,
/// `keycode` is the macOS virtual keycode, `unshifted_codepoint` from
/// `charactersIgnoringModifiers`. Full IME / NSTextInputClient handling (dead
/// keys, marked text) is out of scope for the spike.
/// Returns whether libghostty consumed the event (e.g. as a keybinding like paste/copy) — used by
/// `performKeyEquivalent:` to decide whether to swallow a ⌘-combo or let AppKit route it.
unsafe fn forward_key(
    view: &WardenHostView,
    event: &NSEvent,
    action: ffi::ghostty_input_action_e,
) -> bool {
    let surface = view.ivars().surface.get();
    if surface.is_null() {
        return false;
    }

    let mods = mods_from_event(event);

    // Only forward `text` for genuinely-typed *printable* input (see `is_printable_text`).
    let text = event.characters().map(|s| s.to_string());
    let c_text = text
        .as_deref()
        .filter(|s| is_printable_text(s))
        .and_then(|s| CString::new(s).ok());
    let text_ptr = c_text.as_ref().map_or(ptr::null(), |c| c.as_ptr());

    let unshifted = event
        .charactersIgnoringModifiers()
        .and_then(|s| s.to_string().chars().next())
        .map_or(0u32, |c| c as u32);

    let key = ffi::ghostty_input_key_s {
        action,
        mods,
        consumed_mods: ffi::GHOSTTY_MODS_NONE,
        keycode: event.keyCode() as u32,
        text: text_ptr,
        unshifted_codepoint: unshifted,
        composing: false,
    };
    // `c_text` stays alive until the end of this fn, covering the call.
    ffi::ghostty_surface_key(surface, key)
}

/// Push libghostty the surface's content scale (DPI) and backing pixel size *together*, both
/// derived from the same `scale`. libghostty computes cell metrics from the content scale and
/// sizes its framebuffer from the pixel size, so the two must never drift: a stale content scale
/// against a fresh pixel size makes the grid fill only a fraction of the buffer (the "terminal
/// doesn't fill the window / resizes proportionally after moving to another monitor" bug). Called
/// on creation, on every frame report, and on `viewDidChangeBackingProperties` (a display/scale
/// change) so a backing-scale change always re-pushes both.
unsafe fn apply_surface_geometry(
    surface: ffi::ghostty_surface_t,
    width_pts: f64,
    height_pts: f64,
    scale: f64,
) {
    ffi::ghostty_surface_set_content_scale(surface, scale, scale);
    let (w, h) = geometry::backing_size(
        PixelRect {
            x: 0.0,
            y: 0.0,
            width: width_pts,
            height: height_pts,
        },
        scale,
    );
    ffi::ghostty_surface_set_size(surface, w, h);
}

/// GCD work item: emit a `BackingScaleChanged` signal for the surface passed as `context`
/// (its `*mut ghostty_surface_t` reinterpreted as a `usize` id). Dispatched async from
/// `viewDidChangeBackingProperties` so the respawn it triggers (which frees this surface +
/// its view) runs on a *later* runloop turn, not while the NSView method is still on the
/// stack. If the surface was already torn down by the time this runs, the id simply won't
/// match any live tab and the app drops it — the pointer is only ever compared, never read.
unsafe extern "C" fn scale_changed_trampoline(context: *mut c_void) {
    super::emit_surface_event(SurfaceEvent {
        surface_id: context as usize,
        signal: SurfaceSignal::BackingScaleChanged,
    });
}

// --- GhosttySurface ---------------------------------------------------------
pub struct GhosttySurface {
    host_view: Retained<WardenHostView>,
    window: Retained<NSWindow>,
    surface: ffi::ghostty_surface_t,
}

// SAFETY: see module docs — Tauri-state affordance only; main-thread access.
unsafe impl Send for GhosttySurface {}

impl GhosttySurface {
    /// Opaque identity of this surface — the libghostty surface handle as a `usize`. Matches the
    /// pointer libghostty reports in an action's `target`, so the app layer can route a per-surface
    /// signal (bell / notification) back to the owning tab.
    pub fn id(&self) -> usize {
        self.surface as usize
    }

    /// `ns_window` is the raw `NSWindow *` pointer returned by Tauri's
    /// `WebviewWindow::ns_window()`. The `contentView` is derived here, keeping
    /// all objc2/AppKit calls inside this module (the seam constraint).
    pub fn new(
        ns_window: *mut c_void,
        rect: PixelRect,
        spec: &TabSpec,
    ) -> Result<Self, SurfaceError> {
        let app = shared_app();
        if app.is_null() {
            return Err(SurfaceError::AppCreateFailed);
        }
        // AppKit view creation + all surface methods are main-thread only.
        let mtm =
            MainThreadMarker::new().expect("GhosttySurface::new must be called on the main thread");

        unsafe {
            let window_ref: &NSWindow = &*(ns_window as *const NSWindow);
            let content_view = window_ref
                .contentView()
                .ok_or(SurfaceError::SurfaceCreateFailed)?;
            // Re-derive as Retained<NSWindow> via the view's back-reference so the
            // struct field gets a proper retain count (same as before, just sourced
            // from the raw pointer rather than a pre-derived content_view).
            let window = content_view
                .window()
                .ok_or(SurfaceError::SurfaceCreateFailed)?;
            let scale = window.backingScaleFactor();

            let frame = NSRect::new(
                NSPoint::new(rect.x, rect.y),
                NSSize::new(rect.width, rect.height),
            );

            let host_view: Retained<WardenHostView> =
                msg_send_id![mtm.alloc::<WardenHostView>(), initWithFrame: frame];
            host_view.setWantsLayer(true);

            // Topmost subview => above the WKWebView (which is added by wry first).
            content_view.addSubview(&host_view);

            // Track this window's key state so libghostty's cursor focus follows it (filled in the
            // key window, hollow otherwise) even when no first-responder change fires — see the
            // windowDidBecomeKey:/windowDidResignKey: handlers. Scoped to `window` so each surface
            // only hears its own window; removed in close().
            let center = NSNotificationCenter::defaultCenter();
            center.addObserver_selector_name_object(
                &host_view,
                sel!(windowDidBecomeKey:),
                Some(NSWindowDidBecomeKeyNotification),
                Some(&window),
            );
            center.addObserver_selector_name_object(
                &host_view,
                sel!(windowDidResignKey:),
                Some(NSWindowDidResignKeyNotification),
                Some(&window),
            );

            // Build the surface config from defaults, then override platform/dir/shell/startup.
            let mut cfg = ffi::ghostty_surface_config_new();
            cfg.userdata = ptr::null_mut();
            cfg.platform_tag = ffi::ghostty_platform_e::GHOSTTY_PLATFORM_MACOS;
            cfg.platform = ffi::ghostty_platform_u {
                macos: ffi::ghostty_platform_macos_s {
                    nsview: Retained::as_ptr(&host_view) as *mut c_void,
                },
            };
            cfg.scale_factor = scale;
            cfg.font_size = 0.0; // inherit configured font size

            // Keep CStrings alive across ghostty_surface_new (it copies them).
            let c_dir = CString::new(spec.dir.to_string_lossy().into_owned())
                .map_err(|_| SurfaceError::SurfaceCreateFailed)?;
            let c_shell =
                CString::new(spec.shell.clone()).map_err(|_| SurfaceError::SurfaceCreateFailed)?;
            cfg.working_directory = c_dir.as_ptr();
            cfg.command = c_shell.as_ptr();

            // A tab's startup command is NOT exec'd — it's typed into the interactive shell
            // (newline-terminated so it runs). This is what makes a shell *function* like
            // `amux` resolve and leaves a live shell once the command exits. The CString must
            // outlive ghostty_surface_new (it copies the bytes), so it's bound in this scope.
            let c_startup = spec
                .startup
                .as_ref()
                .map(|s| CString::new(format!("{s}\n")))
                .transpose()
                .map_err(|_| SurfaceError::SurfaceCreateFailed)?;
            if let Some(c) = &c_startup {
                cfg.initial_input = c.as_ptr();
            }

            let surface = ffi::ghostty_surface_new(app, &cfg);
            if surface.is_null() {
                host_view.removeFromSuperview();
                return Err(SurfaceError::SurfaceCreateFailed);
            }
            apply_surface_geometry(surface, rect.width, rect.height, scale);

            // The view forwards keystrokes to this surface for its whole life.
            // Per-view ownership => first-responder routing is correct across
            // windows by construction, no shared global to disambiguate.
            host_view.set_surface(surface);
            // Record the scale the font was built at so viewDidChangeBackingProperties can tell a
            // real DPI change (→ respawn) from a no-op backing-properties fire.
            host_view.set_built_scale(scale);

            // Kick an initial tick in case the first wakeup raced app creation.
            dispatch_async_f(main_queue(), app as *mut c_void, tick_trampoline);

            Ok(GhosttySurface {
                host_view,
                window,
                surface,
            })
        }
    }
}

impl TerminalSurface for GhosttySurface {
    fn set_frame(&self, rect: PixelRect) {
        unsafe {
            let frame = NSRect::new(
                NSPoint::new(rect.x, rect.y),
                NSSize::new(rect.width, rect.height),
            );
            self.host_view.setFrame(frame);
            let scale = self.window.backingScaleFactor();
            apply_surface_geometry(self.surface, rect.width, rect.height, scale);
        }
    }

    fn show(&self) {
        self.host_view.setHidden(false);
    }

    fn hide(&self) {
        self.host_view.setHidden(true);
    }

    fn focus(&self) {
        // &WardenHostView coerces to &NSResponder via the NSView deref chain.
        let responder: &NSResponder = &self.host_view;
        self.window.makeFirstResponder(Some(responder));
        // This surface is now the clipboard-read (paste) target.
        FOCUSED_SURFACE.store(self.surface, Ordering::Release);
        unsafe {
            ffi::ghostty_surface_set_focus(self.surface, true);
            ffi::ghostty_app_set_focus(shared_app(), true);
        }
    }

    fn close(self) {
        // Drop the key-state observers registered in new() before the view is freed, so the
        // notification center stops messaging a dangling view.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.host_view) };
        // Stop targeting this surface for paste before freeing it (avoid completing a request
        // against a dangling pointer); only clear if it's the one currently focused.
        let _ = FOCUSED_SURFACE.compare_exchange(
            self.surface,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        unsafe {
            // The host view is dropped with this struct, so its surface ivar
            // (a dangling pointer after the free) is never read again.
            ffi::ghostty_surface_free(self.surface);
            self.host_view.removeFromSuperview();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_printable_text;

    #[test]
    fn printable_text_excludes_control_chars() {
        // Genuinely-typed text is forwarded as `text`.
        assert!(is_printable_text("a"));
        assert!(is_printable_text("é"));
        assert!(is_printable_text("hello"));
        // Ctrl-combos arrive as C0 control chars — must NOT be forwarded as text, or
        // libghostty emits Kitty `CSI…u` sequences instead of the bare control byte.
        assert!(!is_printable_text("\u{06}")); // Ctrl-F
        assert!(!is_printable_text("\u{03}")); // Ctrl-C
        assert!(!is_printable_text("\u{1b}")); // Esc
        assert!(!is_printable_text("")); // no text at all
    }
}
