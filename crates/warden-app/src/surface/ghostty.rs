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

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{
    class, define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSCursor,
    NSEvent, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF,
    NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowDidBecomeKeyNotification, NSWindowDidChangeScreenNotification,
    NSWindowDidResignKeyNotification,
};
use objc2_foundation::{
    MainThreadMarker, NSData, NSDictionary, NSNotification, NSNotificationCenter, NSObject,
    NSPoint, NSRect, NSSize, NSString,
};

/// Opaque `CGColorRef`. A msg_send returning or taking one (`-[NSColor CGColor]`,
/// `-[CALayer setBackgroundColor:]`) must carry the exact Objective-C type encoding
/// `^{CGColor=}`, or objc2's runtime type-check aborts the send ("expected return to have
/// type code '^{CGColor=}', but found '@'"). A bare `*mut AnyObject` encodes as `@` and is
/// rejected; this zero-sized struct gives the pointer the right encoding.
#[repr(C)]
struct CGColor {
    _private: [u8; 0],
}
unsafe impl objc2::encode::RefEncode for CGColor {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("CGColor", &[]));
}

// --- AppKit modifier-flag bit masks (stable AppKit ABI values) ---
const NS_FLAG_CAPS: usize = 1 << 16;
const NS_FLAG_SHIFT: usize = 1 << 17;
const NS_FLAG_CONTROL: usize = 1 << 18;
const NS_FLAG_OPTION: usize = 1 << 19;
const NS_FLAG_COMMAND: usize = 1 << 20;

// macOS virtual keycode for the physical grave/backtick key (kVK_ANSI_Grave). This is the key
// macOS's window-cycle shortcut lives on; matching by keycode keeps the reservation below correct
// across keyboard layouts (the shortcut follows the physical key, not the "`" character).
const NS_KEYCODE_GRAVE: u16 = 0x32;

/// Corner radius (AppKit points) of a surface's render layer. The chrome's pane ring
/// (`--pane-radius` in ui/index.html, mirrored by `--hole-radius` in shell-core's detach.html)
/// is the WINDOW's own corner — Apple's CONTINUOUS curve at 16pt, measured on macOS 26 off the
/// window's alpha mask, not a circle — so a pane corner that IS a window corner curves with it;
/// this is that radius less the ring's width (`--ring-w`, 1px — the border the surface sits
/// inside), so the surface's curve sits concentrically inside the ring's; the inset squircle at
/// r − w is a true 1:1 offset of the outer to within 1.5%, measured. The layer's corner CURVE is
/// set to continuous beside the radius (in `new`):
/// the page draws the ring with Apple's own per-corner Béziers, and CALayer's continuous curve
/// is the same construction, so the surface's edge and the ring's inner edge coincide. A
/// circular layer corner under a continuous ring (or vice versa) shows as a sliver of ring or
/// ground at every window corner. Restated here rather than read from the page because the two
/// live on opposite sides of the native/web seam (same precedent as the layer ground colour set
/// beside it in `new`, which matches `--pane-ground`); `surface_corner_radius_matches_the_page_tokens` fails if they part. WHICH corners
/// round is not a constant — see [`apply_corner_mask`].
const SURFACE_CORNER_RADIUS: f64 = 15.0;

/// How close (points) a surface's edge must sit to the window's edge to count as touching it,
/// for [`apply_corner_mask`]. The ring's border (`--ring-w`) insets every edge-hugging surface
/// by its width, the inward pixel snap (`geometry::snap_inward`) can add a device pixel, and
/// web→AppKit rect conversion can land on a fraction; 3pt covers a ring up to 2px with all of
/// that, with no risk of a pane on the far side of the 5px divider strip (inset ≥ 6pt) ever
/// qualifying.
const CORNER_TOUCH_EPS: f64 = 3.0;

/// Round ONLY the corners of a surface that coincide with the window's own corners — the
/// chrome's rule (`.pane`/`.hole` per-corner radii) restated on the native side, derived here
/// from geometry rather than plumbed through the rect report: a corner is a window corner iff
/// BOTH its edges touch the content view's matching edges. So a pane's corners at the divider,
/// against the sidebar, or under the detached window's banner stay square, and the two sides of
/// the seam agree by construction (the chrome can only round a corner that is at the window's
/// edge, which is exactly what this measures). Called wherever the frame is set — `new`,
/// `set_frame`, `reparent` — since a resize, a split, or a move to another window can change
/// which corners qualify. AppKit's non-flipped coordinates: `MinY` is the BOTTOM.
///
/// SAFETY: main thread; `host_view` is in a window (so `superview` is its content view) and
/// its layer exists (libghostty made it layer-hosting in `ghostty_surface_new`).
unsafe fn apply_corner_mask(host_view: &WardenHostView) {
    // CACornerMask bits.
    const MIN_X_MIN_Y: usize = 1 << 0;
    const MAX_X_MIN_Y: usize = 1 << 1;
    const MIN_X_MAX_Y: usize = 1 << 2;
    const MAX_X_MAX_Y: usize = 1 << 3;
    let Some(content) = host_view.superview() else {
        return;
    };
    let layer: *mut AnyObject = msg_send![host_view, layer];
    if layer.is_null() {
        return;
    }
    let f = host_view.frame();
    let b = content.bounds();
    let left = f.origin.x <= b.origin.x + CORNER_TOUCH_EPS;
    let right = f.origin.x + f.size.width >= b.origin.x + b.size.width - CORNER_TOUCH_EPS;
    let bottom = f.origin.y <= b.origin.y + CORNER_TOUCH_EPS;
    let top = f.origin.y + f.size.height >= b.origin.y + b.size.height - CORNER_TOUCH_EPS;
    let mut mask = 0usize;
    if left && bottom {
        mask |= MIN_X_MIN_Y;
    }
    if right && bottom {
        mask |= MAX_X_MIN_Y;
    }
    if left && top {
        mask |= MIN_X_MAX_Y;
    }
    if right && top {
        mask |= MAX_X_MAX_Y;
    }
    let _: () = msg_send![layer, setMaskedCorners: mask];
}

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
    pub(super) fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}
pub(super) fn main_queue() -> *mut c_void {
    ptr::addr_of!(_dispatch_main_q) as *mut c_void
}
/// GCD work item: runs on the main thread, ticks the app passed as `context`.
unsafe extern "C" fn tick_trampoline(context: *mut c_void) {
    if !context.is_null() {
        ffi::ghostty_app_tick(context as ffi::ghostty_app_t);
    }
}

/// A `SurfaceEvent` in flight to a later main-queue turn, with the host view it came from.
struct Deferred {
    view: Retained<WardenHostView>,
    event: SurfaceEvent,
}

/// Emit `event` on a *later* main-thread turn than the call that produced it (see `action_cb`
/// and the focus/user-input paths for why each defers). The event is routed by surface address,
/// and a freed surface's address can be reused by the next `ghostty_surface_new` before the turn
/// runs — so the view rides along, and the trampoline drops the event unless the view still hosts
/// that surface (teardown nulls the ivar). A null `view` drops the event now.
unsafe fn defer_event(view: *const WardenHostView, event: SurfaceEvent) {
    let Some(view) = Retained::retain(view as *mut WardenHostView) else {
        return;
    };
    let deferred = Box::new(Deferred { view, event });
    dispatch_async_f(
        main_queue(),
        Box::into_raw(deferred) as *mut c_void,
        emit_event_trampoline,
    );
}

/// GCD work item for [`defer_event`]: emit the event if its surface is still the view's.
unsafe extern "C" fn emit_event_trampoline(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let Deferred { view, event } = *Box::from_raw(context as *mut Deferred);
    if view.ivars().surface.get() as usize == event.surface_id {
        super::emit_surface_event(event);
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
/// on the attention signals (`RING_BELL`, `DESKTOP_NOTIFICATION`), the dead-child signal
/// (`SHOW_CHILD_EXITED`), and the link affordances (`OPEN_URL`, `MOUSE_SHAPE`, `PWD`), decoding
/// each into a seam-neutral `SurfaceEvent` or an AppKit call. All other actions are unhandled;
/// returning false = "not handled", which the reference (`Ghostty.App.swift`) also does for every
/// unimplemented action. Runs on the main thread (called from a `ghostty_app_tick`).
unsafe extern "C" fn action_cb(
    _app: ffi::ghostty_app_t,
    target: ffi::ghostty_target_s,
    action: ffi::ghostty_action_s,
) -> bool {
    // Only surface-targeted signals map to a tab; app-level targets have nowhere to route.
    let Some(surface) = target.surface() else {
        return false;
    };
    // The link actions are handled entirely inside the seam — they act on AppKit, not on a tab, so
    // there is nothing for the app layer to route. A link click always answers `true`, even for a
    // URL that can't be decoded: `false` makes libghostty retry with its own unscreened opener.
    if action.is_open_url() {
        if let (Some((kind, url)), Some(view)) = (action.open_url(), view_for(surface)) {
            let base = view.ivars().link_base.borrow().clone();
            super::link::open(kind, &url, &base);
        }
        return true;
    }
    // The shell's OSC 7 working directory: what a relative path a link names is relative to.
    if let Some(pwd) = action.pwd() {
        if let Some(view) = view_for(surface) {
            *view.ivars().link_base.borrow_mut() = PathBuf::from(pwd);
        }
        return true;
    }
    if let Some(shape) = action.mouse_shape() {
        set_mouse_shape(surface, shape);
        return true;
    }

    // The app-layer signals all reach warden on the *next* main-queue turn, never inside this
    // callback. A dead child tears the tab down, which FREES this very surface — and libghostty is
    // still using it: `Surface.childExited` calls us mid-function and, once we answer `true`, goes
    // on to read `self.config` and call `self.close()` (Surface.zig), so freeing here is a silent
    // use-after-free. Bell and notification free nothing but lock `ManagerState` (`notify.rs`),
    // which must stay off the lock path of any action libghostty might raise synchronously from a
    // warden call that already holds it.
    let signal = if let Some(exit_code) = action.child_exited() {
        SurfaceSignal::ChildExited {
            exit_code: Some(exit_code),
        }
    } else if action.is_ring_bell() {
        SurfaceSignal::Bell
    } else if let Some(dn) = action.desktop_notification() {
        // Copy the borrowed C strings out now — libghostty frees them when this call returns.
        let read = |p: *const c_char| {
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        SurfaceSignal::Notification {
            title: read(dn.title),
            body: read(dn.body),
        }
    } else {
        return false;
    };
    let view = SURFACE_VIEWS
        .lock()
        .ok()
        .and_then(|m| m.get(&(surface as usize)).copied())
        .unwrap_or(0);
    defer_event(
        view as *const WardenHostView,
        SurfaceEvent {
            surface_id: surface as usize,
            signal,
        },
    );
    true
}

/// The surface libghostty currently believes focused, so a focus change can clear the previous
/// one (`performKeyEquivalent:`). This is the one piece of process-global surface state (keys and
/// clipboard requests route via the per-view ivar instead). Written by `focus()` (tab activate)
/// and refreshed in `performKeyEquivalent:` / `becomeFirstResponder:` (so focus that arrives by
/// clicking another window — never through activate — still lands). Cleared by
/// `resignFirstResponder:` and on `close()`, so a freed surface is never handed focus (UAF).
static FOCUSED_SURFACE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Monotonic counter for temp image-paste filenames (see `clipboard_image_to_temp_path`).
static PASTE_IMAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// The last image spill: the pasteboard `changeCount` it was written for, and its path.
static SPILLED_IMAGE: Mutex<Option<(isize, String)>> = Mutex::new(None);

/// One temp file per clipboard image: reuse `cache`'s path while the pasteboard's `change_count`
/// is the one it was spilled for and the file is still there, else `spill` and remember the result.
/// A program looping clipboard reads therefore writes one file, not one per read.
fn reuse_or_spill(
    cache: &mut Option<(isize, String)>,
    change_count: isize,
    spill: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if let Some((count, path)) = cache.as_ref() {
        if *count == change_count && std::path::Path::new(path).is_file() {
            return Some(path.clone());
        }
    }
    let path = spill()?;
    *cache = Some((change_count, path.clone()));
    Some(path)
}

/// A surface asks for clipboard data (⌘V, or a program's OSC 52 / Kitty read); read the macOS
/// general pasteboard and hand it back to *that* surface via `complete_clipboard_request`,
/// synchronously. `userdata` is the requesting surface's host view — a background tab's OSC 52
/// read is answered into its own PTY, never the focused tab's. Runs on the main thread (from a
/// `ghostty_app_tick`).
///
/// warden serves only `text/plain` — the type every paste and OSC 52 read asks for. A Kitty
/// clipboard read wanting another type gets no representation for it, and a listing (mode 5522)
/// advertises `text/plain` alone, so no program is told about a type it can't then read.
///
/// Text wins. If the clipboard is image-only (a screenshot, a copied image — no text), spill it to
/// a temp PNG and paste that *path* instead — Claude Code (and friends) pick up a pasted image off
/// bracketed paste exactly the way drag-and-drop delivers a file path. The image bytes never transit
/// the PTY; the consuming program reads the file itself. This is what makes ⌘V-a-screenshot work.
///
/// Completes with `confirmed: false`, so libghostty applies its own policy (the user's Ghostty
/// `clipboard-read`, `clipboard-paste-protection`) and routes anything needing approval through
/// `confirm_read_clipboard_cb`, which allows only a user paste.
unsafe extern "C" fn read_clipboard_cb(
    userdata: *mut c_void,
    loc: ffi::ghostty_clipboard_e,
    state: *mut c_void,
    mimes: *const *const c_char,
    mimes_len: usize,
    list: bool,
) -> ffi::ghostty_clipboard_read_result_e {
    use ffi::{
        GHOSTTY_CLIPBOARD_READ_STARTED, GHOSTTY_CLIPBOARD_READ_UNAVAILABLE,
        GHOSTTY_CLIPBOARD_READ_UNSUPPORTED,
    };
    if loc != ffi::GHOSTTY_CLIPBOARD_STANDARD {
        return GHOSTTY_CLIPBOARD_READ_UNSUPPORTED;
    }
    let surface = surface_of(userdata);
    if surface.is_null() {
        return GHOSTTY_CLIPBOARD_READ_UNAVAILABLE;
    }
    let wanted: &[*const c_char] = if mimes.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(mimes, mimes_len)
    };
    let wants_text = wanted
        .iter()
        .any(|&m| !m.is_null() && std::ffi::CStr::from_ptr(m).to_bytes() == b"text/plain");
    if !wants_text && !list {
        return GHOSTTY_CLIPBOARD_READ_UNAVAILABLE;
    }

    // The image spill runs only when the text is delivered. A listing-only request still advertises
    // `text/plain` for an image-only clipboard (the follow-up read spills it) but writes no file.
    let pb = NSPasteboard::generalPasteboard();
    let text = pb
        .stringForType(NSPasteboardTypeString)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let payload = match text {
        Some(t) => Some(t),
        None if wants_text => {
            let mut cache = SPILLED_IMAGE.lock().unwrap_or_else(|e| e.into_inner());
            reuse_or_spill(&mut cache, pb.changeCount(), || {
                clipboard_image_to_temp_path(&pb)
            })
        }
        None => (pb.dataForType(NSPasteboardTypePNG).is_some()
            || pb.dataForType(NSPasteboardTypeTIFF).is_some())
        .then(String::new),
    };
    let Some(payload) = payload else {
        return GHOSTTY_CLIPBOARD_READ_UNAVAILABLE;
    };

    let text_plain = c"text/plain";
    let content = ffi::ghostty_clipboard_content_s {
        mime: text_plain.as_ptr(),
        data: payload.as_ptr().cast(),
        len: payload.len(),
    };
    let available = [text_plain.as_ptr()];
    let complete = ffi::ghostty_clipboard_complete_s {
        contents: if wants_text { &content } else { ptr::null() },
        contents_len: wants_text as usize,
        available: if list {
            available.as_ptr()
        } else {
            ptr::null()
        },
        available_len: if list { available.len() } else { 0 },
        confirmed: false,
        remember: false,
    };
    ffi::ghostty_surface_complete_clipboard_request(surface, &complete, state);
    GHOSTTY_CLIPBOARD_READ_STARTED
}

/// If the general pasteboard carries raster image data (and no text), write it to a temp PNG and
/// return that path; `None` when there's no image. PNG is emitted unconditionally — consumers match
/// on image *extensions*, and macOS screenshots land on the clipboard as TIFF, so a TIFF-only
/// clipboard is transcoded first. `reuse_or_spill` makes this one file per clipboard image; those
/// accrete in the temp dir for the session, and the OS reaps them.
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
                f.write_all(&png.to_vec()).ok()?;
                return Some(path.to_str()?.to_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}
/// libghostty wants the user to approve a clipboard transfer. warden has no prompt UI, so the answer
/// is fixed by request kind:
///
/// - **A user paste (⌘V) is approved** — completed with the payload's own contents, `confirmed:
///   true`. It reaches here only when paste protection flags the text unsafe (a newline outside
///   bracketed paste); the user asked for this paste, so it goes through as it always has.
/// - **Everything else is denied** — an OSC 52 / Kitty read (a program, possibly over ssh, reading
///   the clipboard), or a Kitty write under `clipboard-write = ask`. The deny sends the protocol's
///   own refusal reply to the program.
///
/// Answering is mandatory: an unanswered request leaks its state.
unsafe extern "C" fn confirm_read_clipboard_cb(
    userdata: *mut c_void,
    confirm: *const ffi::ghostty_clipboard_confirm_s,
    state: *mut c_void,
    request: ffi::ghostty_clipboard_request_e,
) {
    let surface = surface_of(userdata);
    if surface.is_null() {
        return;
    }
    // Any request kind but a recognised paste — including a value this build doesn't know — is
    // denied.
    if request == ffi::GHOSTTY_CLIPBOARD_REQUEST_PASTE && !confirm.is_null() {
        let c = &*confirm;
        let complete = ffi::ghostty_clipboard_complete_s {
            contents: c.contents,
            contents_len: c.contents_len,
            available: c.available,
            available_len: c.available_len,
            confirmed: true,
            remember: false,
        };
        ffi::ghostty_surface_complete_clipboard_request(surface, &complete, state);
    } else {
        ffi::ghostty_surface_deny_clipboard_request(surface, state);
    }
}
/// Copy: libghostty hands us the selected text (e.g. on ⌘C or copy-on-select); write it to the
/// macOS general pasteboard. macOS has no primary-selection clipboard, so we ignore SELECTION
/// writes. Runs on the main thread (called from a `ghostty_app_tick`).
///
/// `confirm` is set for a program's OSC 52 write under the user's Ghostty `clipboard-write = ask`
/// (user copies pass `false`). warden has no prompt to show, so a write needing approval is dropped
/// — the same answer `confirm_read_clipboard_cb` gives a program's read.
unsafe extern "C" fn write_clipboard_cb(
    _userdata: *mut c_void,
    loc: ffi::ghostty_clipboard_e,
    content: *const ffi::ghostty_clipboard_content_s,
    len: usize,
    confirm: bool,
) {
    if confirm || loc != ffi::GHOSTTY_CLIPBOARD_STANDARD || content.is_null() || len == 0 {
        return;
    }
    // Take the first entry that carries valid UTF-8 text (usually the text/plain mime). `data` is
    // `(ptr, len)`, not NUL-terminated.
    let entries = std::slice::from_raw_parts(content, len);
    let Some(text) = entries.iter().find_map(|e| {
        (!e.data.is_null())
            .then(|| {
                std::str::from_utf8(std::slice::from_raw_parts(e.data.cast::<u8>(), e.len)).ok()
            })
            .flatten()
    }) else {
        return;
    };
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
}
/// libghostty asks the app to close a surface — for warden, that IS a child exit, reached only as
/// the FALLBACK: `Surface.childExited` (pinned source) first offers every exit, normal or abnormal,
/// as the `SHOW_CHILD_EXITED` action, which `action_cb` takes and unloads on at once. Only if the
/// app declines it does libghostty print "Process exited. Press any key to close the terminal."
/// and, on that key, call this. Wired to the same `ChildExited` so a declined action (a tag the
/// decoder doesn't know) still ends the pane rather than stranding the overlay — but if this is
/// the path that fires, the action match is broken; fix that, don't lean on this.
///
/// `userdata` is the surface's own host view (set in `new`) — the only identity this callback
/// carries; its `surface` ivar is the handle the app layer routes on. `process_alive == true` is
/// a close *request* for a live process that libghostty wants confirmed, and is ignored. A
/// `close_surface` keybind on a terminal needing no confirmation (idle at a prompt, or
/// `confirm-close-surface = false`) arrives with `process_alive == false` and ends the pane like
/// an exit — which is why `performKeyEquivalent:` keeps ⌘W, libghostty's default for it, away
/// from libghostty (`is_close_chord`).
/// Deferred to the next main-queue turn exactly like the action path: this is called from inside
/// libghostty, which is still standing on the surface.
unsafe extern "C" fn close_surface_cb(userdata: *mut c_void, process_alive: bool) {
    if process_alive {
        return;
    }
    let surface = surface_of(userdata);
    if surface.is_null() {
        return;
    }
    defer_event(
        userdata as *const WardenHostView,
        SurfaceEvent {
            surface_id: surface as usize,
            signal: SurfaceSignal::ChildExited { exit_code: None },
        },
    );
}

/// The surface behind a per-surface callback's `userdata` — its host view (set in `new`), whose
/// `surface` ivar is the handle. Null for a null view or a surface already torn down.
unsafe fn surface_of(userdata: *mut c_void) -> ffi::ghostty_surface_t {
    if userdata.is_null() {
        return ptr::null_mut();
    }
    let view: &WardenHostView = &*(userdata as *const WardenHostView);
    view.ivars().surface.get()
}

// --- Shared app -------------------------------------------------------------
/// The user's ghostty config (if any), finalized — the reference's Config(at:) sequence. Null on
/// failure; the caller owns it.
unsafe fn user_config() -> ffi::ghostty_config_t {
    let config = ffi::ghostty_config_new();
    if !config.is_null() {
        ffi::ghostty_config_load_default_files(config);
        ffi::ghostty_config_finalize(config);
    }
    config
}

/// The terminal's font size in points, as libghostty resolves it from the user's config (its
/// default when unset). Read once; a webview CSS px is a point, so the chrome sizes its own text
/// with it.
pub fn terminal_font_size() -> f32 {
    static SIZE: OnceLock<f32> = OnceLock::new();
    *SIZE.get_or_init(|| unsafe {
        const KEY: &str = "font-size";
        const FALLBACK: f32 = 13.0;
        let config = user_config();
        if config.is_null() {
            return FALLBACK;
        }
        let mut size: f32 = 0.0;
        let ok = ffi::ghostty_config_get(
            config,
            (&mut size as *mut f32).cast(),
            KEY.as_ptr().cast(),
            KEY.len(),
        );
        ffi::ghostty_config_free(config);
        if ok && size > 0.0 {
            size
        } else {
            FALLBACK
        }
    })
}

/// Build the ghostty app exactly once. Returns 0 on failure.
unsafe fn create_app() -> usize {
    // ghostty_app_new takes ownership of the config, so we deliberately do not free it.
    let config = user_config();
    if config.is_null() {
        return 0;
    }

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

/// Surface handle → its `WardenHostView`, so an action that names a *surface* can act on the view
/// hosting it. Only `MOUSE_SHAPE` needs this today: a mouse cursor is a view-level AppKit concern,
/// not a tab-level one, so it can't ride the `SurfaceEvent` sink like bell/notification do.
/// Main-thread only (action_cb runs on the main thread, from `ghostty_app_tick`); the pointer is
/// stored as a `usize` purely so the map is `Send`. Entries are removed in `close()`, so a stale
/// view is never dereferenced.
static SURFACE_VIEWS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

/// Apply libghostty's requested cursor shape to the view hosting `surface`. Stores it on the view
/// and invalidates its cursor rects — AppKit re-asks via `resetCursorRects`, which is what makes the
/// shape *stick* as the mouse moves (a bare `NSCursor::set()` is undone by the next mouse-move).
/// The host view `surface` renders into, from `SURFACE_VIEWS`.
///
/// # Safety
/// Main thread only, and the reference must not outlive the current callback: the entry is removed
/// in `close()` before the view dies.
unsafe fn view_for<'a>(surface: ffi::ghostty_surface_t) -> Option<&'a WardenHostView> {
    let ptr = SURFACE_VIEWS
        .lock()
        .ok()
        .and_then(|m| m.get(&(surface as usize)).copied())?;
    Some(&*(ptr as *const WardenHostView))
}

fn set_mouse_shape(surface: ffi::ghostty_surface_t, shape: u32) {
    // SAFETY: main thread (action_cb), used only within this call.
    let Some(view) = (unsafe { view_for(surface) }) else {
        return;
    };
    if view.ivars().mouse_shape.get() == shape {
        return;
    }
    view.ivars().mouse_shape.set(shape);
    if let Some(window) = view.window() {
        window.invalidateCursorRectsForView(view);
    }
}

/// Map `ghostty_action_mouse_shape_e` onto AppKit's cursors. The shapes are CSS-derived, so several
/// have no AppKit equivalent — those fall back to the arrow rather than approximating. The two that
/// actually matter in a terminal: `TEXT` over cells (I-beam) and `POINTER` over a link (hand).
fn cursor_for_shape(shape: u32) -> Retained<NSCursor> {
    // ghostty.h `ghostty_action_mouse_shape_e`, 0-based in declaration order.
    const POINTER: u32 = 3;
    const CROSSHAIR: u32 = 7;
    const TEXT: u32 = 8;
    const ALIAS: u32 = 10;
    const COPY: u32 = 11;
    const NOT_ALLOWED: u32 = 14;
    const GRAB: u32 = 15;
    const GRABBING: u32 = 16;
    const COL_RESIZE: u32 = 18;
    const ROW_RESIZE: u32 = 19;
    {
        match shape {
            POINTER => NSCursor::pointingHandCursor(),
            TEXT => NSCursor::IBeamCursor(),
            CROSSHAIR => NSCursor::crosshairCursor(),
            ALIAS => NSCursor::dragLinkCursor(),
            COPY => NSCursor::dragCopyCursor(),
            NOT_ALLOWED => NSCursor::operationNotAllowedCursor(),
            GRAB => NSCursor::openHandCursor(),
            GRABBING => NSCursor::closedHandCursor(),
            // Their replacements (`columnResizeCursorInDirections:` and kin) are macOS 15+.
            #[allow(deprecated)]
            COL_RESIZE => NSCursor::resizeLeftRightCursor(),
            #[allow(deprecated)]
            ROW_RESIZE => NSCursor::resizeUpDownCursor(),
            _ => NSCursor::arrowCursor(),
        }
    }
}

// --- Custom NSView that forwards keyboard events to libghostty --------------
define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WardenHostView"]
    #[ivars = HostIvars]
    struct WardenHostView;

    impl WardenHostView {
        #[unsafe(method_id(initWithFrame:))]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> Option<Retained<Self>> {
            let this = this.set_ivars(HostIvars {
                surface: Cell::new(ptr::null_mut()),
                mouse_shape: Cell::new(0),
                tracking_area: RefCell::new(None),
                rex: Cell::new(false),
                menu_press: Cell::new(0),
                link_base: RefCell::new(PathBuf::new()),
            });
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

        // Must be true for the view to accept key events as first responder.
        #[unsafe(method(acceptsFirstResponder))]
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
        // cursor state now tracks the real first responder. Also record it in FOCUSED_SURFACE, consistent
        // with performKeyEquivalent:.
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            let surface = self.ivars().surface.get();
            if !surface.is_null() {
                FOCUSED_SURFACE.store(surface, Ordering::Release);
                unsafe { ffi::ghostty_surface_set_focus(surface, true) };
                // Report the focus change to the app layer (which pane of which tab is now
                // typing). DEFERRED to the next main-queue turn, never delivered here: this
                // method also runs synchronously inside `focus()` → `makeFirstResponder`, which
                // `Registry::activate` calls while the `ManagerState` lock is held — a sink that
                // re-locked from inside it would deadlock. Same trampoline as the child-exit
                // path, for the same reason.
                unsafe {
                    defer_event(
                        self,
                        SurfaceEvent {
                            surface_id: surface as usize,
                            signal: SurfaceSignal::Focused,
                        },
                    )
                };
                // Focus landing in the key window is a claim edge for a Rex pane; in a
                // background window it isn't yet — `windowDidBecomeKey:` raises it then.
                if self.window().is_some_and(|w| w.isKeyWindow()) {
                    self.user_input();
                }
            }
            true
        }

        // The counterpart of becomeFirstResponder:, and the one place a surface loses libghostty
        // focus within its window. AppKit calls it on the view giving up first responder: a click
        // into a sibling split pane, focus()'s makeFirstResponder onto another tab or pane, the
        // sidebar webview taking keys. Without it the previous surface stays focused — two solid
        // cursors in a split, two 60fps display links, no focus-out report for vim/tmux. Clearing
        // FOCUSED_SURFACE (when it names this surface) keeps performKeyEquivalent:'s handoff from
        // re-clearing a surface that already let go. Window key changes leave the first responder
        // alone; the key observers below cover those.
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            let surface = self.ivars().surface.get();
            if !surface.is_null() {
                let _ = FOCUSED_SURFACE.compare_exchange(
                    surface,
                    ptr::null_mut(),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                unsafe { ffi::ghostty_surface_set_focus(surface, false) };
            }
            true
        }

        // becomeFirstResponder: only fires on a *change* of first responder — AppKit doesn't re-send
        // it when a window merely re-becomes key with its old first responder intact. So switching
        // apps/windows away and back left libghostty's focus flag stale: a surface stayed focused
        // (filled cursor) in a now-non-key window, or stayed hollow after the window came back. These
        // two observers (registered per-window in `new`, scoped via `object: window`) track key state
        // directly. Every tab view in a window receives the notification; only the one that owns
        // the window's keys reacts (`owns_window_keys` — the same disambiguation
        // performKeyEquivalent: uses, so a split's two visible panes never both claim focus).
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            let surface = self.ivars().surface.get();
            // SAFETY: notifications are delivered on the main thread.
            if surface.is_null() || !unsafe { self.owns_window_keys() } {
                return;
            }
            FOCUSED_SURFACE.store(surface, Ordering::Release);
            unsafe { ffi::ghostty_surface_set_focus(surface, true) };
            self.user_input();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            let surface = self.ivars().surface.get();
            if surface.is_null() || self.isHidden() {
                return;
            }
            unsafe { ffi::ghostty_surface_set_focus(surface, false) };
        }

        #[unsafe(method(windowDidChangeScreen:))]
        fn window_did_change_screen(&self, _notification: &NSNotification) {
            let surface = self.ivars().surface.get();
            if surface.is_null() {
                return;
            }
            // Rebind libghostty's render display link to the window's current display so vsync
            // tracks THIS monitor's refresh rate. A window moved between mixed-refresh displays
            // otherwise keeps the creation display's link → judder / frames the panel can't show.
            // A scale change from the same move is handled by viewDidChangeBackingProperties.
            // SAFETY: notifications are delivered on the main thread.
            let Some(window) = self.window() else { return };
            unsafe { ffi::ghostty_surface_set_display_id(surface, window_display_id(&window)) };
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let unmodified = event.charactersIgnoringModifiers().map(|s| s.to_string());
            if !key_down_forwards(event.modifierFlags().0, unmodified.as_deref()) {
                return;
            }
            self.user_input();
            // A held key is a repeat, not a fresh press — Kitty event-type reporting tells them apart.
            let action = if event.isARepeat() {
                ffi::ghostty_input_action_e::GHOSTTY_ACTION_REPEAT
            } else {
                ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS
            };
            unsafe { forward_key(self, event, action) };
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            unsafe { forward_key(self, event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_RELEASE) };
        }

        // A modifier pressed or released alone arrives here, never at keyDown:/keyUp:. Without it
        // libghostty learns mods only from the next key or mouse move, so ⌘-over-a-link doesn't
        // underline until the mouse moves, and modifier-only key reports are never sent.
        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            unsafe { forward_modifier(self, event) };
        }

        // The Edit menu's Copy / Paste / Select All. The menu has first refusal on ⌘C / ⌘V / ⌘A
        // (performKeyEquivalent: below), so its standard actions reach the focused terminal here
        // and run libghostty's own bindings, as Ghostty's macOS app does. Undo, Redo and Cut have
        // no responder here, so AppKit disables them while a terminal is focused.
        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            binding_action(self, "copy_to_clipboard");
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            self.paste_clipboard();
        }

        #[unsafe(method(selectAll:))]
        fn select_all(&self, _sender: Option<&AnyObject>) {
            binding_action(self, "select_all");
        }

        // ⌘-combo key-DOWN events arrive via performKeyEquivalent:, NOT keyDown:. Two owners can
        // want them: the app menu (tab nav ⌘⇧[/⌘⇧], ⌘1–9, ⌘Q/⌘W/…) and libghostty's own keybinds.
        // macOS consults the view hierarchy BEFORE the main menu, and libghostty binds the very
        // same standard tab chords — so forwarding first let it swallow them (consumed=true) and
        // the menu never fired (the "inconsistent tab switch" bug). Give the main menu first
        // refusal: if it owns the chord, let it act and stop (the Edit menu's ⌘C/⌘V/⌘A come back
        // to this view as copy:/paste:/selectAll:). Otherwise forward to libghostty's own
        // keybinds and return whether it consumed — if not, return NO so AppKit can still
        // route ⌘` and friends. The menu's accelerators define the reserved set (self-maintaining).
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            if let Some(mtm) = MainThreadMarker::new() {
                let main_menu = NSApplication::sharedApplication(mtm).mainMenu();
                if let Some(menu) = main_menu {
                    if menu.performKeyEquivalent(event) {
                        return objc2::runtime::Bool::YES;
                    }
                }
            }

            // ⌘` / ⌘⇧` is macOS's own "cycle windows of this app" chord — a system window-
            // management shortcut no terminal should eat. Never forward it to libghostty: a surface
            // in the Kitty keyboard protocol (a running TUI turned it on) WOULD encode ⌘` and report
            // consumed=true, so warden returned YES and AppKit's cycling never ran — the chord died
            // silently on whatever tab had such a program running, while bare-shell tabs cycled fine.
            // Hand it straight back to AppKit (return NO) so cycling is uniform across every tab.
            // warden owns no cmd+` binding of its own by design — window cycling is macOS's, and
            // fighting it would be a losing battle. SAFETY: delivered on the main thread.
            if unsafe { is_window_cycle_chord(event) } {
                return objc2::runtime::Bool::NO;
            }
            // ⌘W is warden's menu's alone, even while its item is disabled (`is_close_chord`).
            let unmodified = event.charactersIgnoringModifiers().map(|s| s.to_string());
            if is_close_chord(event.modifierFlags().0, unmodified.as_deref()) {
                return objc2::runtime::Bool::NO;
            }

            let surface = self.ivars().surface.get();
            // Disambiguate sibling views — every host view in the window receives this event
            // (see `owns_window_keys` for the rule and why it is NOT a FOCUSED_SURFACE match).
            // SAFETY: performKeyEquivalent: is delivered on the main thread.
            if surface.is_null() || !unsafe { self.owns_window_keys() } {
                return objc2::runtime::Bool::NO;
            }
            // This chord proves THIS surface is the focused one. Hand libghostty focus over from
            // whatever surface held it — only on a real change, to avoid resetting cursor blink
            // each press.
            let prev = FOCUSED_SURFACE.swap(surface, Ordering::AcqRel);
            if prev != surface {
                unsafe {
                    if !prev.is_null() {
                        ffi::ghostty_surface_set_focus(prev, false);
                    }
                    ffi::ghostty_surface_set_focus(surface, true);
                }
            }
            // Arrows, F-keys and ⌘-chords the menu didn't take are typing too.
            self.user_input();
            let consumed =
                unsafe { forward_key(self, event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS) };
            objc2::runtime::Bool::new(consumed)
        }

        // --- Mouse: forward button/drag/scroll so terminal mouse modes (tmux pane select,
        // scrollback, TUI clicks) work, plus *motion* (below) so libghostty can track what's under
        // the cursor. ---

        // Hover position. Without this libghostty never learns where the mouse is between clicks,
        // so it can't resolve the cell under the cursor — which is what its link detection runs on.
        // That made URLs completely inert: no hover underline, no pointer cursor, and no
        // GHOSTTY_ACTION_OPEN_URL on click, since there was no hovered link to open. Motion needs an
        // NSTrackingArea (AppKit doesn't deliver mouseMoved: otherwise), installed in
        // `updateTrackingAreas` below.
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }

        // Entering the view means the pointer has just left the window's CHROME — and that is the
        // only place it can be observed. This view composites above the webview and takes the
        // pointer at the boundary, so on a fast flick out of the sidebar the page gets no further
        // event and its CSS `:hover` freezes where it was (symptom: the de-emphasised main list
        // stays lit). Report it so the app layer can tell the sidebar. Emitted directly, not via
        // `defer_event`, which the focus/child-exit paths use: AppKit delivers this
        // from the event loop, never re-entrantly from inside our own `ManagerState` lock.
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            let surface = self.ivars().surface.get();
            if surface.is_null() {
                return;
            }
            unsafe { settle_webview_tooltip(self, event) };
            super::emit_surface_event(SurfaceEvent {
                surface_id: surface as usize,
                signal: SurfaceSignal::PointerEntered,
            });
        }

        // Leaving the view must clear libghostty's hover, or the last hovered link keeps its
        // underline while the mouse sits in the sidebar.
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            let surface = self.ivars().surface.get();
            if surface.is_null() {
                return;
            }
            // Negative coordinates = "outside the surface" (Ghostty.app clears hover the same way).
            unsafe { ffi::ghostty_surface_mouse_pos(surface, -1.0, -1.0, mouse_mods_from_event(event)) };
        }

        // AppKit calls this on every geometry change; the tracking area must be rebuilt each time
        // (and the previous one removed, or they stack). `InVisibleRect` keeps the region pinned to
        // the visible bounds, and `ActiveInKeyWindow` limits hover tracking to the focused window —
        // a click into a background window still lands correctly, because `forward_mouse_button`
        // pushes the position before the button.
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                if let Some(old) = self.ivars().tracking_area.borrow_mut().take() {
                    self.removeTrackingArea(&old);
                }
                let area = NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.bounds(),
                    NSTrackingAreaOptions::MouseEnteredAndExited
                        | NSTrackingAreaOptions::MouseMoved
                        | NSTrackingAreaOptions::ActiveInKeyWindow
                        | NSTrackingAreaOptions::InVisibleRect,
                    Some(self),
                    None,
                );
                self.addTrackingArea(&area);
                *self.ivars().tracking_area.borrow_mut() = Some(area);
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
        }

        // The cursor shape libghostty asked for (I-beam over cells, hand over a link). Cursor
        // *rects* rather than a bare NSCursor::set(): AppKit re-applies these as the mouse moves,
        // so the shape sticks instead of being reset by the next motion event.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let cursor = cursor_for_shape(self.ivars().mouse_shape.get());
            self.addCursorRect_cursor(self.bounds(), &cursor);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            self.user_input();
            // ⌃-click is the Mac's right-click: warden's menu, never a selection start.
            let ctrl = event.modifierFlags().0 & NS_FLAG_CONTROL != 0;
            let forward = if ctrl {
                self.route_menu_press(event, Button::Left)
            } else {
                begin_press(&self.ivars().menu_press, Button::Left, Route::Program)
            };
            if forward {
                unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_LEFT) };
            }
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            if end_press(&self.ivars().menu_press, Button::Left) {
                unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_LEFT) };
            }
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            self.user_input();
            if self.route_menu_press(event, Button::Right) {
                unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_RIGHT) };
            }
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            if end_press(&self.ivars().menu_press, Button::Right) {
                unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_RIGHT) };
            }
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            self.user_input();
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_MIDDLE) };
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            use ffi::{ghostty_input_mouse_button_e::*, ghostty_input_mouse_state_e::*};
            unsafe { forward_mouse_button(self, event, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_MIDDLE) };
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            unsafe { forward_mouse_pos(self, event) };
        }
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            unsafe { forward_scroll(self, event) };
        }

        // Fired by AppKit when the view's backing properties change — specifically when the window
        // moves to a screen with a *different* backingScaleFactor (laptop Retina 2× ↔ external 1×).
        // Re-push content scale + size so libghostty's cell metrics track the new DPI; without this
        // the surface keeps the creation-time scale and the terminal stops filling the window (and
        // a later resize scales the grid proportionally instead of reflowing). A plain window *move*
        // between same-scale displays doesn't fire this, which is correct — nothing to update.
        #[unsafe(method(viewDidChangeBackingProperties))]
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
    /// Cursor shape libghostty last asked for (`ghostty_action_mouse_shape_e`), read back by
    /// `resetCursorRects`. Defaults to 0 (`DEFAULT` → arrow).
    mouse_shape: Cell<u32>,
    /// The view's live `NSTrackingArea`, held so `updateTrackingAreas` can remove the old one
    /// before installing a replacement (AppKit would otherwise stack duplicates on every resize).
    tracking_area: RefCell<Option<Retained<NSTrackingArea>>>,
    /// Whether this surface shows a Rex pane (its spec has a `server`), so it raises
    /// `SurfaceSignal::UserInput`. Set once in `new`; a plain pane leaves it false.
    rex: Cell<bool>,
    /// One bit per [`Button`] whose current press opened warden's menu, so that button's release
    /// is swallowed too (`begin_press` / `end_press`).
    menu_press: Cell<u8>,
    /// The directory a relative link target resolves against: the tab's `dir`, then each OSC 7
    /// working directory the shell reports (`action_cb`).
    link_base: RefCell<PathBuf>,
}

/// A mouse button the menu routing tracks; its discriminant is its bit in `menu_press`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Left = 1,
    Right = 2,
}

/// Where a right-click (or ⌃-click) goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// warden's menu at the click: libghostty sees neither press nor release.
    Menu,
    /// The terminal program, as any other button press.
    Program,
}

/// A program that captured the mouse (tmux, vim, less) gets its right-click; otherwise the click
/// opens warden's menu. ⌃ always opens the menu — the escape hatch past a capturing program.
fn right_click_route(captured: bool, ctrl: bool) -> Route {
    if ctrl || !captured {
        Route::Menu
    } else {
        Route::Program
    }
}

/// Record a press's route; true when the press goes to libghostty. Every press of a button
/// overwrites that button's bit, so a menu press whose release the popped menu's tracking loop
/// consumed can't swallow the button's next release. Per button, so a menu opened while the
/// other button is held leaves that button's release forwarded.
fn begin_press(menu_press: &Cell<u8>, button: Button, route: Route) -> bool {
    let bit = button as u8;
    let others = menu_press.get() & !bit;
    menu_press.set(if route == Route::Menu {
        others | bit
    } else {
        others
    });
    route == Route::Program
}

/// True when this button's release goes to libghostty — i.e. its press did.
fn end_press(menu_press: &Cell<u8>, button: Button) -> bool {
    let bit = button as u8;
    let flags = menu_press.get();
    menu_press.set(flags & !bit);
    flags & bit == 0
}

impl WardenHostView {
    /// Paste the clipboard, as typed input.
    fn paste_clipboard(&self) {
        self.user_input();
        binding_action(self, "paste_from_clipboard");
    }

    /// Route a right-button press, or a left one with ⌃, by [`right_click_route`]; true when the
    /// caller forwards it to libghostty. A menu press focuses this pane as a click does — AppKit
    /// only makes a view first responder on a left press, so the right button needs it done —
    /// then raises `ContextMenu` at the click, DEFERRED like `user_input` (this runs inside an
    /// AppKit event method, which can arrive with `ManagerState` held).
    fn route_menu_press(&self, event: &NSEvent, button: Button) -> bool {
        let surface = self.ivars().surface.get();
        if surface.is_null() {
            // No surface to ask or forward to; still clear the button's bit like any press.
            begin_press(&self.ivars().menu_press, button, Route::Program);
            return false;
        }
        let ctrl = event.modifierFlags().0 & NS_FLAG_CONTROL != 0;
        let captured = unsafe { ffi::ghostty_surface_mouse_captured(surface) };
        let route = right_click_route(captured, ctrl);
        if begin_press(&self.ivars().menu_press, button, route) {
            return true;
        }
        let Some(window) = self.window() else {
            return false;
        };
        window.makeFirstResponder(Some(self));
        let Some(content) = window.contentView() else {
            return false;
        };
        let local = content.convertPoint_fromView(event.locationInWindow(), None);
        // The bottom-left → top-left flip is its own inverse, so the web → view converter
        // applied to a zero-size rect at the click maps it the other way.
        let at = geometry::web_rect_to_view(
            geometry::WebRect {
                x: local.x,
                y: local.y,
                width: 0.0,
                height: 0.0,
            },
            content.bounds().size.height,
        );
        unsafe {
            defer_event(
                self,
                SurfaceEvent {
                    surface_id: surface as usize,
                    signal: SurfaceSignal::ContextMenu { x: at.x, y: at.y },
                },
            )
        };
        false
    }

    fn set_surface(&self, surface: ffi::ghostty_surface_t) {
        self.ivars().surface.set(surface);
    }

    /// Raise `UserInput` for a Rex pane; nothing for a plain one. DEFERRED to the next main-queue
    /// turn like `Focused`: it is called from inside AppKit event methods — and from
    /// `becomeFirstResponder`, which `Registry::activate` reaches with `ManagerState` held — so a
    /// handler that locked the manager here would re-enter it.
    fn user_input(&self) {
        let surface = self.ivars().surface.get();
        if surface.is_null() || !self.ivars().rex.get() {
            return;
        }
        unsafe {
            defer_event(
                self,
                SurfaceEvent {
                    surface_id: surface as usize,
                    signal: SurfaceSignal::UserInput,
                },
            )
        };
    }

    /// Is this the ONE host view in its window that window-scoped key traffic belongs to —
    /// `performKeyEquivalent:` (⌘-chords AND every function key: arrows, F-keys, Home/End —
    /// AppKit routes those through the key-equivalent pass, not `keyDown:`) and the
    /// `windowDidBecomeKey:` focus restore? Both are delivered to EVERY host view in the window,
    /// so without a single answer the first subview in order (the primary pane) claims them.
    ///
    /// The rule, in two parts:
    /// 1. Hidden views never own keys — the inactive tabs of a window (show()/hide()).
    /// 2. Among the unhidden ones, if the window's first responder IS a host view, only that
    ///    view owns keys. A split shows two unhidden panes; the typing one is the first
    ///    responder (`becomeFirstResponder` keeps libghostty in step with AppKit), so this is
    ///    exactly "the pane the user is in". The bug this replaced: the gate was visibility
    ///    alone (written when one view per window was ever unhidden), so with a split the
    ///    primary's view answered every arrow key first, stole libghostty focus to itself, and
    ///    the arrow landed in the OTHER terminal — "pressing → in the left pane moves the cursor
    ///    to the right pane" — while plain typing (`keyDown:`, first-responder-routed) stayed
    ///    put.
    ///
    /// 3. If the first responder is some OTHER view — the sidebar's webview (its search field,
    ///    or a click anywhere in the chrome) — no host view owns keys: returning NO from
    ///    `performKeyEquivalent:` lets AppKit deliver the arrow to the webview's `keyDown:`, the
    ///    same place plain typing already goes. Otherwise ↑/↓ in the search drove the terminal.
    ///
    /// With no view as first responder (the window itself), every unhidden view qualifies: only
    /// the key window's hierarchy receives these events, so an unhidden view here is the
    /// focused terminal. This is deliberately not a FOCUSED_SURFACE match — that pointer can
    /// name a surface in ANOTHER window, and gating on it once made ⌘C/⌘V dead in a window
    /// focused by click until a tab row was clicked. In a split in that state the primary wins
    /// the tie, which is also the pane `focus_pane` defaults to.
    ///
    /// SAFETY: must run on the main thread (reads AppKit window state).
    unsafe fn owns_window_keys(&self) -> bool {
        if self.isHidden() {
            return false;
        }
        let Some(window) = self.window() else {
            return false;
        };
        match window.firstResponder() {
            Some(fr) if fr.isKindOfClass(WardenHostView::class()) => {
                let me: &NSResponder = self;
                ptr::eq(&*fr as *const NSResponder, me as *const NSResponder)
            }
            Some(fr) if fr.isKindOfClass(NSView::class()) => false,
            _ => true,
        }
    }
}

/// True when `s` is genuinely-typed printable text — the only case we forward as the key
/// event's `text` to libghostty. macOS `characters` for a Ctrl/Cmd combo is the C0 control
/// char (Ctrl-F → "\u{06}"); handing *that* to libghostty as `text` makes it emit a Kitty
/// `CSI…u` sequence (e.g. `^[[6;5u`) instead of the bare control byte (`^F`) that legacy apps —
/// tmux, less, vim — read as the keypress. Ghostty's own macOS app omits `text` for these and
/// lets the key encoder derive the byte from key + mods.
///
/// The same holds for AppKit's function-key characters (U+F700–U+F8FF: arrows, F-keys,
/// Home/End): they are private-use, not control, chars, and in the Kitty keyboard protocol
/// libghostty encodes that `text` instead of the key, so ↑/↓ reach a Kitty-mode program (fish's
/// prompt, Claude Code) as nothing it recognises. Ghostty's macOS app drops the same range.
fn is_printable_text(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c))
}

/// True for ⌘` / ⌘⇧` — macOS's "cycle windows of this app" chord. Matched by physical keycode
/// (not the "`" character) so it holds across keyboard layouts, and gated on Command with no
/// Ctrl/Option so only the genuine window-cycle chord is reserved (Shift is allowed — ⌘⇧` cycles
/// the other direction). `performKeyEquivalent:` uses this to hand the chord back to AppKit rather
/// than forward it to libghostty.
unsafe fn is_window_cycle_chord(event: &NSEvent) -> bool {
    let flags = event.modifierFlags().0;
    flags & NS_FLAG_COMMAND != 0
        && flags & NS_FLAG_CONTROL == 0
        && flags & NS_FLAG_OPTION == 0
        && event.keyCode() == NS_KEYCODE_GRAVE
}

/// True for ⌘W with any of ⇧ / ⌥ (`flags` an AppKit modifier mask, `unmodified` the event's
/// `charactersIgnoringModifiers`): Close Tab / Pane, Close Project Tab and Close Window are
/// warden's menu's, and the combos with no warden item (⇧⌥⌘W) are kept from libghostty too,
/// whose bindings on them close surfaces or windows. Matched by character, as the menu matches
/// its own key equivalents. Neither `performKeyEquivalent:` nor `keyDown:` (`key_down_forwards`)
/// forwards one to libghostty, whose default `close_surface` would otherwise end the pane
/// whenever warden's item is disabled.
fn is_close_chord(flags: usize, unmodified: Option<&str>) -> bool {
    flags & NS_FLAG_COMMAND != 0
        && flags & NS_FLAG_CONTROL == 0
        && unmodified.is_some_and(|s| s.eq_ignore_ascii_case("w"))
}

/// Whether `keyDown:` forwards a press to libghostty. AppKit delivers a key equivalent nothing
/// claimed (a disabled ⌘W item, ⇧⌥⌘W) to the first responder's `keyDown:` after
/// `performKeyEquivalent:` declines it, so a close chord is dropped here as well.
fn key_down_forwards(flags: usize, unmodified: Option<&str>) -> bool {
    !is_close_chord(flags, unmodified)
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

/// The mods a key event's `text` already accounts for: ⇧, when there is printable text. In the
/// Kitty keyboard protocol an unconsumed ⇧ makes libghostty report the key as `CSI <key>;2u`
/// instead of its text, and without the report-alternates flag that report doesn't carry the
/// shifted character, so ⇧' reaches fish as a key with no `"`. ⌥ stays unconsumed so
/// `macos-option-as-alt` still turns ⌥-keys into Alt; ⌃ and ⌘ never produce the text.
fn consumed_mods(mods: ffi::ghostty_input_mods_e, has_text: bool) -> ffi::ghostty_input_mods_e {
    if has_text {
        mods & ffi::GHOSTTY_MODS_SHIFT
    } else {
        ffi::GHOSTTY_MODS_NONE
    }
}

/// The mods a mouse event reports: ⌘ also carries ⇧. Under mouse reporting (Claude Code, tmux
/// `mouse on`) libghostty hands clicks to the program unless ⇧ is held, and a mouse report can't
/// carry ⌘ — so a ⌘-click would reach the program as a bare click. Adding ⇧ keeps ⌘-hover and
/// ⌘-click with libghostty, so ⌘-click opens a link everywhere. Mouse path only: key events keep
/// their real mods.
fn mouse_mods(mods: ffi::ghostty_input_mods_e) -> ffi::ghostty_input_mods_e {
    if mods & ffi::GHOSTTY_MODS_SUPER != 0 {
        mods | ffi::GHOSTTY_MODS_SHIFT
    } else {
        mods
    }
}

unsafe fn mouse_mods_from_event(event: &NSEvent) -> ffi::ghostty_input_mods_e {
    mouse_mods(mods_from_event(event))
}

/// Hand the window's WKWebView one mouse move at the point the pointer entered this view, so
/// WebKit re-hit-tests the page there (the untitled `#terminal-hole`) and drops the `title`
/// tooltip it was holding. WebKit registers every tooltip over a wide-open rect (±100000pt) and
/// clears it only on a move it sees; its tracking observer ignores moves while a sibling view is
/// topmost under the cursor, which this view always is. So leaving a titled element straight onto
/// a terminal kept the tooltip armed, and AppKit popped it wherever the cursor next rested — over
/// the terminal, far from its element. `_simulateMouseMove:` is WebKit's own entry past that
/// topmost gate (WKWebViewMac.mm); a WebKit without it just keeps the old behaviour.
unsafe fn settle_webview_tooltip(view: &WardenHostView, event: &NSEvent) {
    let Some(superview) = view.superview() else {
        return;
    };
    let Some(wk) = objc2::runtime::AnyClass::get(c"WKWebView") else {
        return;
    };
    let Some(webview) = superview
        .subviews()
        .iter()
        .find(|v| v.isKindOfClass(wk) && v.respondsToSelector(sel!(_simulateMouseMove:)))
    else {
        return;
    };
    let moved = mouse_moved_event(
        event.locationInWindow(),
        event.modifierFlags(),
        event.timestamp(),
        event.windowNumber(),
    );
    if let Some(moved) = moved {
        let _: () = msg_send![&*webview, _simulateMouseMove: &*moved];
    }
}

/// A synthetic `NSEventTypeMouseMoved` at `location` (window coordinates).
fn mouse_moved_event(
    location: NSPoint,
    flags: objc2_app_kit::NSEventModifierFlags,
    timestamp: f64,
    window_number: isize,
) -> Option<Retained<NSEvent>> {
    unsafe {
        msg_send![
            class!(NSEvent),
            mouseEventWithType: objc2_app_kit::NSEventType::MouseMoved,
            location: location,
            modifierFlags: flags,
            timestamp: timestamp,
            windowNumber: window_number,
            context: std::ptr::null_mut::<AnyObject>(),
            eventNumber: 0isize,
            clickCount: 0isize,
            pressure: 0f32,
        ]
    }
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
    ffi::ghostty_surface_mouse_pos(
        surface,
        local.x,
        height - local.y,
        mouse_mods_from_event(event),
    );
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
    ffi::ghostty_surface_mouse_button(surface, state, button, mouse_mods_from_event(event));
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
/// Runs libghostty's named keybinding `action` on `view`'s surface; a view with no surface yet
/// does nothing.
fn binding_action(view: &WardenHostView, action: &str) {
    let surface = view.ivars().surface.get();
    if surface.is_null() {
        return;
    }
    // SAFETY: a live surface; `action` outlives the call and `len` is its byte length.
    unsafe {
        ffi::ghostty_surface_binding_action(surface, action.as_ptr().cast(), action.len());
    }
}

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
        consumed_mods: consumed_mods(mods, c_text.is_some()),
        keycode: event.keyCode() as u32,
        text: text_ptr,
        unshifted_codepoint: unshifted,
        composing: false,
    };
    // `c_text` stays alive until the end of this fn, covering the call.
    ffi::ghostty_surface_key(surface, key)
}

/// Forward a `flagsChanged:` event as a press or release of the modifier key it names: a press
/// when that modifier's bit is now set. Carries no text — `characters` raises on a
/// flags-changed event, so this can't go through [`forward_key`]. Same keycode table as
/// Ghostty.app's `flagsChanged(with:)`.
unsafe fn forward_modifier(view: &WardenHostView, event: &NSEvent) {
    let surface = view.ivars().surface.get();
    if surface.is_null() {
        return;
    }
    let bit = match event.keyCode() {
        0x39 => ffi::GHOSTTY_MODS_CAPS,
        0x38 | 0x3C => ffi::GHOSTTY_MODS_SHIFT,
        0x3B | 0x3E => ffi::GHOSTTY_MODS_CTRL,
        0x3A | 0x3D => ffi::GHOSTTY_MODS_ALT,
        0x37 | 0x36 => ffi::GHOSTTY_MODS_SUPER,
        _ => return,
    };
    let mods = mods_from_event(event);
    let action = if mods & bit != 0 {
        ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS
    } else {
        ffi::ghostty_input_action_e::GHOSTTY_ACTION_RELEASE
    };
    let key = ffi::ghostty_input_key_s {
        action,
        mods,
        consumed_mods: ffi::GHOSTTY_MODS_NONE,
        keycode: event.keyCode() as u32,
        text: ptr::null(),
        unshifted_codepoint: 0,
        composing: false,
    };
    ffi::ghostty_surface_key(surface, key);
}

/// The `CGDirectDisplayID` of the display a window currently sits on (0 if unknown). Read from
/// the screen's `deviceDescription["NSScreenNumber"]` — the same source Ghostty.app uses.
/// libghostty binds its render display link to this ID so vsync tracks that monitor's refresh
/// rate; without it a window keeps the *creation* display's link and, once moved to a
/// mixed-refresh second monitor, judders / presents frames the panel can't show.
/// SAFETY: main-thread-only AppKit reads.
unsafe fn window_display_id(window: &NSWindow) -> u32 {
    let Some(screen) = window.screen() else {
        return 0;
    };
    let desc: *mut AnyObject = msg_send![&*screen, deviceDescription];
    if desc.is_null() {
        return 0;
    }
    let key = NSString::from_str("NSScreenNumber");
    let value: *mut AnyObject = msg_send![desc, objectForKey: &*key];
    if value.is_null() {
        return 0;
    }
    msg_send![value, unsignedIntValue]
}

/// Wire a host view, already a subview of `window`'s content view and hosting `surface`, to that
/// window — the per-window half of `GhosttySurface::new`, re-run by `reparent` after
/// `removeObserver` drops the old window's:
///
/// - **Observers** for the window's key changes (libghostty's cursor focus follows the key
///   window even when no first-responder change fires) and screen changes (the render display
///   link's vsync display). Scoped to `window`, so a view hears only its own; removed in `close()`.
/// - **Display id**, seeded now so vsync matches the window's monitor from the first frame.
/// - **Focus**, seeded from the REAL key state instead of libghostty's focused=1 default, which
///   runs a 60fps render display link. The key observers keep it right afterwards, but a window
///   that is never key (every background window at launch, a background pop-out) emits neither,
///   and a surface left focused there catches the text cursor mid-redraw — the "cursor flashes
///   around an unfocused window" bug. Ghostty.app derives focus as `NSApp.isActive &&
///   window.isKeyWindow`; this mirrors it.
///
/// SAFETY: main thread; `surface` is live.
unsafe fn attach_to_window(
    view: &WardenHostView,
    window: &NSWindow,
    surface: ffi::ghostty_surface_t,
    mtm: MainThreadMarker,
) {
    let center = NSNotificationCenter::defaultCenter();
    for (selector, name) in [
        (sel!(windowDidBecomeKey:), NSWindowDidBecomeKeyNotification),
        (sel!(windowDidResignKey:), NSWindowDidResignKeyNotification),
        (
            sel!(windowDidChangeScreen:),
            NSWindowDidChangeScreenNotification,
        ),
    ] {
        center.addObserver_selector_name_object(view, selector, Some(name), Some(window));
    }
    ffi::ghostty_surface_set_display_id(surface, window_display_id(window));
    let focused = NSApplication::sharedApplication(mtm).isActive() && window.isKeyWindow();
    ffi::ghostty_surface_set_focus(surface, focused);
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

// --- GhosttySurface ---------------------------------------------------------
pub struct GhosttySurface {
    host_view: Retained<WardenHostView>,
    window: Retained<NSWindow>,
    surface: ffi::ghostty_surface_t,
    /// Set once teardown has run (via `close()` or `Drop`), so the two paths can't
    /// double-free the libghostty surface or double-remove the notification observer.
    closed: bool,
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

    /// The terminal grid this surface lays out, `(columns, rows)`, as of its last size push.
    pub fn grid(&self) -> (u16, u16) {
        let s = unsafe { ffi::ghostty_surface_size(self.surface) };
        (s.columns, s.rows)
    }

    /// Whether the terminal holds a text selection (what Copy would take).
    pub fn has_selection(&self) -> bool {
        unsafe { ffi::ghostty_surface_has_selection(self.surface) }
    }

    /// The Edit menu's Copy, run on this surface.
    pub fn copy(&self) {
        binding_action(&self.host_view, "copy_to_clipboard");
    }

    /// The Edit menu's Paste, run on this surface.
    pub fn paste(&self) {
        self.host_view.paste_clipboard();
    }

    /// One cell's size in device pixels, `(width, height)`; `None` while libghostty doesn't know
    /// it, or when it doesn't fit a `u16`.
    pub fn cell_px(&self) -> Option<(u16, u16)> {
        let s = unsafe { ffi::ghostty_surface_size(self.surface) };
        let w = u16::try_from(s.cell_width_px).ok().filter(|&w| w > 0)?;
        let h = u16::try_from(s.cell_height_px).ok().filter(|&h| h > 0)?;
        Some((w, h))
    }

    /// `ns_window` is the raw `NSWindow *` pointer returned by Tauri's
    /// `WebviewWindow::ns_window()`. The `contentView` is derived here, keeping
    /// all objc2/AppKit calls inside this module (the seam constraint).
    pub fn new(
        ns_window: *mut c_void,
        rect: PixelRect,
        spec: &TabSpec,
    ) -> Result<Self, SurfaceError> {
        // No window (headless tests pass null): nothing to host the view in.
        if ns_window.is_null() {
            return Err(SurfaceError::SurfaceCreateFailed);
        }
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
                msg_send![mtm.alloc::<WardenHostView>(), initWithFrame: frame];
            host_view.setWantsLayer(true);

            // Build the fallible CStrings up front — BEFORE wiring the view into AppKit — so an
            // interior NUL in dir/shell/cmd returns Err while host_view is still just a local
            // Retained (dropped cleanly), leaving no zombie subview or dangling observer behind.
            // They must also outlive ghostty_surface_new (it copies the bytes), which this scope
            // guarantees.
            let c_dir = CString::new(spec.dir.to_string_lossy().into_owned())
                .map_err(|_| SurfaceError::SurfaceCreateFailed)?;
            let c_shell =
                CString::new(spec.shell.clone()).map_err(|_| SurfaceError::SurfaceCreateFailed)?;
            // A tab's startup command is NOT exec'd — it's typed into the interactive shell
            // (newline-terminated so it runs). This is what makes a shell *function* like
            // `amux` resolve and leaves a live shell once the command exits.
            let c_startup = spec
                .startup
                .as_ref()
                .map(|s| CString::new(format!("{s}\n")))
                .transpose()
                .map_err(|_| SurfaceError::SurfaceCreateFailed)?;

            // Topmost subview => above the WKWebView (which is added by wry first).
            content_view.addSubview(&host_view);

            // Build the surface config from defaults, then override platform/dir/shell/startup.
            let mut cfg = ffi::ghostty_surface_config_new();
            // The host view is this surface's identity in the one runtime callback that carries
            // no surface handle (`close_surface_cb`); it outlives the surface (dropped with this
            // struct, after `ghostty_surface_free`), so the pointer is valid for every callback.
            cfg.userdata = Retained::as_ptr(&host_view) as *mut c_void;
            cfg.platform_tag = ffi::ghostty_platform_e::GHOSTTY_PLATFORM_MACOS;
            cfg.platform = ffi::ghostty_platform_u {
                macos: ffi::ghostty_platform_macos_s {
                    nsview: Retained::as_ptr(&host_view) as *mut c_void,
                },
            };
            cfg.scale_factor = scale;
            cfg.font_size = 0.0; // inherit configured font size
            cfg.working_directory = c_dir.as_ptr();
            cfg.command = c_shell.as_ptr();
            if let Some(c) = &c_startup {
                cfg.initial_input = c.as_ptr();
            }

            let surface = ffi::ghostty_surface_new(app, &cfg);
            if surface.is_null() {
                // Unwind the subview so a libghostty spawn failure degrades to a clean cold tab.
                host_view.removeFromSuperview();
                return Err(SurfaceError::SurfaceCreateFailed);
            }
            apply_surface_geometry(surface, rect.width, rect.height, scale);

            // The view forwards keystrokes to this surface for its whole life.
            // Per-view ownership => first-responder routing is correct across
            // windows by construction, no shared global to disambiguate.
            host_view.set_surface(surface);
            host_view.ivars().rex.set(spec.server.is_some());
            *host_view.ivars().link_base.borrow_mut() = spec.dir.clone();
            attach_to_window(&host_view, &window, surface, mtm);

            // Back the surface's render layer with an OPAQUE colour so its very first composited
            // frame — before libghostty's Metal renderer presents, and any un-painted region during
            // a resize — is warden's terminal-area colour, not the transparent window hole showing
            // the desktop. libghostty makes this view layer-hosting but, when embedded, does NOT set
            // the layer background; that's the embedder's job, exactly as Ghostty.app's own
            // SurfaceView sets it. Without it, clicking a *cold* tab flashes the wallpaper for the
            // frames between the old (opaque) surface hiding and the new surface's first paint — a
            // sub-frame race the #empty-state placeholder can't win, because it toggles `display` and
            // its composite can lose to the native surface swap on both edges. #0e1516 matches the
            // #empty-state backstop. SAFETY: main thread; `layer` is libghostty's render layer
            // (assigned during ghostty_surface_new above), only messaged, never retained.
            let layer: *mut AnyObject = msg_send![&*host_view, layer];
            if !layer.is_null() {
                let color: *mut AnyObject = msg_send![
                    class!(NSColor),
                    colorWithSRGBRed: 0.0549_f64, green: 0.0824_f64, blue: 0.0863_f64, alpha: 1.0_f64
                ];
                // `CGColor` is autoreleased (Get rule); setBackgroundColor: retains it.
                let cg: *mut CGColor = msg_send![color, CGColor];
                let _: () = msg_send![layer, setBackgroundColor: cg];
                // Round the surface's window corners to sit concentrically inside the chrome's
                // rounded pane ring (`.pane`'s per-corner radii in ui/index.html, `.hole`'s in
                // shell-core's detach.html): the surface composites ABOVE the webview, so without
                // this its square corners paint straight over the ring's curve. `masksToBounds` is
                // what actually clips the Metal contents — `cornerRadius` alone rounds only the
                // layer's own background. WHICH corners: `apply_corner_mask`, from geometry. The
                // corner cost is a sliver of the outermost cells' corners, inside libghostty's
                // default 2pt window padding; the same trade Ghostty.app makes at its own corners.
                // The curve is CONTINUOUS (`kCACornerCurveContinuous`, the string "continuous"),
                // matching the window's own corner and the ring's Béziers — see the
                // `SURFACE_CORNER_RADIUS` doc comment.
                let _: () = msg_send![layer, setCornerRadius: SURFACE_CORNER_RADIUS];
                let curve = NSString::from_str("continuous");
                let _: () = msg_send![layer, setCornerCurve: &*curve];
                let _: () = msg_send![layer, setMasksToBounds: true];
                apply_corner_mask(&host_view);
            }

            // Let a surface-targeted action reach this view (mouse-cursor shape). Removed in
            // close(), which runs before the view is dropped.
            if let Ok(mut map) = SURFACE_VIEWS.lock() {
                map.insert(surface as usize, Retained::as_ptr(&host_view) as usize);
            }

            // Kick an initial tick in case the first wakeup raced app creation.
            dispatch_async_f(main_queue(), app as *mut c_void, tick_trampoline);

            Ok(GhosttySurface {
                host_view,
                window,
                surface,
                closed: false,
            })
        }
    }

    /// Move this LIVE surface — its PTY, scrollback, and running process all intact — into a
    /// different NSWindow, re-parenting the native view at the AppKit level WITHOUT freeing or
    /// respawning the libghostty surface. This is the session-preserving half of "pop a tab out
    /// into its own window": a later task hands the owning tab slot to a new window's registry
    /// and calls this to carry the live view across.
    ///
    /// It drops the old window's observers, moves the host view, then runs `new`'s own
    /// `attach_to_window` against the new window (observers, display id, focus seed). It calls
    /// NONE of `teardown` / `ghostty_surface_free` / `close`: the surface handle and everything it
    /// owns downstream (PTY, scrollback, child process) must survive untouched.
    ///
    /// The `SURFACE_VIEWS` map and `FOCUSED_SURFACE` atomic are deliberately left alone: the map
    /// keys `surface → host_view`, and neither pointer changes across a reparent (same surface,
    /// same view), so the entry stays valid; `FOCUSED_SURFACE` is owned by `focus()`, which the caller
    /// drives after the move — `new` doesn't touch it at spawn either.
    ///
    /// `new_ns_window` is the raw `NSWindow *` (Tauri's `WebviewWindow::ns_window()`), as in
    /// `new`. `rect` is the view's frame in the destination content-view's coordinates, applied
    /// immediately so the surface fills the new window's hole with no blank first frame.
    ///
    /// Main-thread only, like every other method here (AppKit + libghostty surface calls).
    pub fn reparent(
        &mut self,
        new_ns_window: *mut c_void,
        rect: PixelRect,
    ) -> Result<(), SurfaceError> {
        // A torn-down surface has a freed handle and its observers already removed; reparenting
        // it would message a dangling view / a freed surface. Refuse rather than corrupt state.
        if self.closed || new_ns_window.is_null() {
            return Err(SurfaceError::SurfaceCreateFailed);
        }
        let mtm = MainThreadMarker::new()
            .expect("GhosttySurface::reparent must be called on the main thread");

        unsafe {
            // Derive the destination Retained<NSWindow> exactly as new() does: from the raw
            // NSWindow* via its contentView's back-reference, so the swapped-in field carries a
            // proper retain count.
            let window_ref: &NSWindow = &*(new_ns_window as *const NSWindow);
            let content_view = window_ref
                .contentView()
                .ok_or(SurfaceError::SurfaceCreateFailed)?;
            let new_window = content_view
                .window()
                .ok_or(SurfaceError::SurfaceCreateFailed)?;

            // Drop the OLD window's observers: `attach_to_window` registered them all with
            // `host_view` as the observer, so one removeObserver drops them — as teardown does.
            NSNotificationCenter::defaultCenter().removeObserver(&self.host_view);

            // Move the native view: out of the old content view, back in as the TOPMOST subview
            // of the new one (above its WKWebView — same as new()'s addSubview).
            self.host_view.removeFromSuperview();
            content_view.addSubview(&self.host_view);

            // Un-hide the view. A reparent target ALWAYS shows this surface: the detached window
            // exists to show its one tab, and a redock immediately re-`activate`s the returned
            // tab. Load-bearing for popping out a NON-active tab: that tab's surface was
            // `hide()`n (setHidden(true)) when it was deselected in the origin, and without this
            // it would stay hidden in the detached window — only the banner would appear, no
            // terminal (the "unselected tab pops out to just a title bar" bug).
            self.host_view.setHidden(false);

            // Adopt the new window as this surface's owner (replaces the old retain), fit the view
            // to its hole now — the new display's scale with it — so there is no blank first frame,
            // and rewire the per-window state against it.
            self.window = new_window;
            self.set_frame(rect);
            attach_to_window(&self.host_view, &self.window, self.surface, mtm);
        }

        Ok(())
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
            apply_corner_mask(&self.host_view);
            let scale = self.window.backingScaleFactor();
            apply_surface_geometry(self.surface, rect.width, rect.height, scale);
        }
    }

    fn show(&self) {
        self.host_view.setHidden(false);
    }

    fn hide(&self) {
        self.host_view.setHidden(true);
        // Clearing focus is what STOPS the surface's 60fps render display link; hiding the view
        // alone does not (a hidden surface keeps rendering, just off-screen). This is the
        // symmetric counterpart to `focus()` and the only path that covers a tab going
        // off-screen: `windowDidResignKey:` and the `performKeyEquivalent:` handoff both gate on
        // `!isHidden()`, and `resignFirstResponder:` fires only for the view that held first
        // responder — so without this line a tab not holding it when switched away from stays
        // focused=1 and burns 60fps for the life of the process. Safe
        // against the active tab: `activate`'s show()+focus() and hide() branches are mutually
        // exclusive, so hide() only ever lands on a non-active surface.
        unsafe { ffi::ghostty_surface_set_focus(self.surface, false) };
    }

    fn focus(&self) {
        // &WardenHostView coerces to &NSResponder via the NSView deref chain.
        let responder: &NSResponder = &self.host_view;
        self.window.makeFirstResponder(Some(responder));
        FOCUSED_SURFACE.store(self.surface, Ordering::Release);
        unsafe {
            ffi::ghostty_surface_set_focus(self.surface, true);
            ffi::ghostty_app_set_focus(shared_app(), true);
        }
    }

    fn run_command(&self, cmd: &str) {
        unsafe {
            // 1) Inject the command text. Length-delimited buffer (not a C string) → embedded NULs
            //    are fine and no CString needed. This lands like a paste, so it must NOT include the
            //    newline: a shell in bracketed-paste mode would insert it literally, not run the line.
            ffi::ghostty_surface_text(self.surface, cmd.as_ptr() as *const c_char, cmd.len());
            // 2) Submit with a real Enter key event (macOS virtual keycode 36, unshifted codepoint
            //    CR) — Ghostty's key encoder turns this into the correct submit bytes for the
            //    surface's current keyboard mode, which a bare `\n` can't. PRESS does the encoding;
            //    RELEASE mirrors a real keypress.
            let enter = |action| ffi::ghostty_input_key_s {
                action,
                mods: ffi::GHOSTTY_MODS_NONE,
                consumed_mods: ffi::GHOSTTY_MODS_NONE,
                keycode: 36, // kVK_Return
                text: ptr::null(),
                unshifted_codepoint: 0x0d, // CR — what the Return key reports
                composing: false,
            };
            ffi::ghostty_surface_key(
                self.surface,
                enter(ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS),
            );
            ffi::ghostty_surface_key(
                self.surface,
                enter(ffi::ghostty_input_action_e::GHOSTTY_ACTION_RELEASE),
            );
        }
    }

    fn close(mut self) {
        self.teardown();
        // `self` drops here; `Drop` sees `closed == true` and no-ops.
    }
}

impl GhosttySurface {
    /// Idempotent teardown shared by `close()` and the `Drop` safety net. Guarded by
    /// `closed` so the two paths can't double-free the surface or double-remove the
    /// observer. Must run before the `Retained` fields drop (releasing their retains),
    /// which they do right after — matching the original `close()` ordering.
    fn teardown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Drop the surface→view entry before the view is freed, so a late MOUSE_SHAPE action can't
        // dereference a dangling view (same lifetime discipline as the observer removal below).
        if let Ok(mut map) = SURFACE_VIEWS.lock() {
            map.remove(&(self.surface as usize));
        }
        // Drop the key-state observers registered in new() before the view is freed, so the
        // notification center stops messaging a dangling view.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.host_view) };
        // Forget this surface before freeing it, so a later focus change never touches a dangling
        // pointer; only clear if it's the one currently focused.
        let _ = FOCUSED_SURFACE.compare_exchange(
            self.surface,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        // Null the view's surface ivar before the free: AppKit can still message the view
        // (resignFirstResponder: as it leaves the window), and every reader bails on null.
        self.host_view.set_surface(ptr::null_mut());
        unsafe {
            ffi::ghostty_surface_free(self.surface);
            self.host_view.removeFromSuperview();
        }
    }
}

impl Drop for GhosttySurface {
    /// Safety net: every Registry teardown path calls `close()`, but a surface dropped
    /// *without* it (e.g. a future `Vec` op on the tab registry, or `ManagerState` teardown
    /// at process exit) would otherwise leak the libghostty surface/PTY and leave a dangling
    /// notification observer the center keeps messaging — a latent use-after-free. `teardown`
    /// is idempotent (guarded by `closed`), so this is a no-op after `close()`.
    fn drop(&mut self) {
        if !self.closed {
            #[cfg(debug_assertions)]
            log::error!(
                "GhosttySurface dropped without close() — tearing down in Drop (bug: a \
                 registry path bypassed close())"
            );
            self.teardown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        begin_press, consumed_mods, end_press, is_close_chord, is_printable_text,
        key_down_forwards, mouse_mods, right_click_route, Button, Route, NS_FLAG_COMMAND,
        NS_FLAG_CONTROL, NS_FLAG_OPTION, NS_FLAG_SHIFT,
    };
    use crate::ffi;
    use std::cell::Cell;

    /// The tooltip settle's synthetic move is a real mouse-moved event at the entry point —
    /// and its `msg_send` passes objc2's runtime encoding check.
    #[test]
    fn mouse_moved_event_is_a_move_at_the_point() {
        let at = objc2_foundation::NSPoint::new(12.0, 34.0);
        let e = super::mouse_moved_event(at, objc2_app_kit::NSEventModifierFlags::empty(), 1.0, 0)
            .expect("event");
        assert_eq!(e.r#type(), objc2_app_kit::NSEventType::MouseMoved);
        assert_eq!(e.locationInWindow(), at);
    }

    /// ⇧ that produced the text is consumed, so a Kitty-mode program gets `"` rather than
    /// `CSI 39;2u`; ⌥, ⌃ and ⌘ never are, and a key with no text consumes nothing.
    #[test]
    fn shift_with_text_is_consumed() {
        let s = ffi::GHOSTTY_MODS_SHIFT;
        assert_eq!(consumed_mods(s, true), s);
        assert_eq!(consumed_mods(s | ffi::GHOSTTY_MODS_ALT, true), s);
        assert_eq!(
            consumed_mods(ffi::GHOSTTY_MODS_CTRL, true),
            ffi::GHOSTTY_MODS_NONE
        );
        assert_eq!(consumed_mods(s, false), ffi::GHOSTTY_MODS_NONE);
    }

    /// ⌘ on a mouse event also reports ⇧, so a ⌘-click stays with libghostty (link open) even
    /// under mouse reporting; anything without ⌘ is passed through untouched.
    #[test]
    fn mouse_command_also_reports_shift() {
        let cmd = ffi::GHOSTTY_MODS_SUPER;
        assert_eq!(mouse_mods(cmd), cmd | ffi::GHOSTTY_MODS_SHIFT);
        assert_eq!(
            mouse_mods(cmd | ffi::GHOSTTY_MODS_ALT),
            cmd | ffi::GHOSTTY_MODS_ALT | ffi::GHOSTTY_MODS_SHIFT
        );
        for m in [
            ffi::GHOSTTY_MODS_NONE,
            ffi::GHOSTTY_MODS_SHIFT,
            ffi::GHOSTTY_MODS_CTRL,
            ffi::GHOSTTY_MODS_ALT,
        ] {
            assert_eq!(mouse_mods(m), m);
        }
    }

    /// ⌘W in every form warden's menu owns (⌘W, ⌥⌘W, ⌘⇧W) never reaches libghostty, whose own
    /// `close_surface` would end the pane even while warden's item is disabled.
    #[test]
    fn close_chord_is_warden_s() {
        let cmd = NS_FLAG_COMMAND;
        assert!(is_close_chord(cmd, Some("w")));
        assert!(is_close_chord(cmd | NS_FLAG_OPTION, Some("w")));
        assert!(is_close_chord(cmd | NS_FLAG_SHIFT, Some("W")));
        assert!(!is_close_chord(0, Some("w")), "a bare w is typing");
        assert!(!is_close_chord(cmd | NS_FLAG_CONTROL, Some("w")));
        assert!(!is_close_chord(cmd, Some("q")));
        assert!(!is_close_chord(cmd, None));
    }

    /// AppKit hands a key equivalent nothing claimed (⌘W with its item disabled, ⇧⌥⌘W with no
    /// item at all) to the first responder's `keyDown:`; that path keeps every close chord from
    /// libghostty too, and still forwards everything else.
    #[test]
    fn key_down_never_forwards_a_close_chord() {
        let cmd = NS_FLAG_COMMAND;
        assert!(!key_down_forwards(cmd, Some("w")));
        assert!(!key_down_forwards(
            cmd | NS_FLAG_SHIFT | NS_FLAG_OPTION,
            Some("W")
        ));
        assert!(key_down_forwards(0, Some("w")));
        assert!(key_down_forwards(cmd, Some("q")));
        assert!(key_down_forwards(0, None));
    }

    /// `SURFACE_CORNER_RADIUS` restates the page's `--pane-radius` − `--ring-w` across the
    /// native/web seam; editing either side alone shows as a sliver at every window corner.
    #[test]
    fn surface_corner_radius_matches_the_page_tokens() {
        let page = include_str!("../../ui/index.html");
        let px = |token: &str| -> f64 {
            let at = page.find(&format!("{token}:")).expect(token) + token.len() + 1;
            let rest = page[at..].trim_start();
            rest[..rest.find("px").expect("px value")]
                .parse()
                .expect(token)
        };
        assert_eq!(
            super::SURFACE_CORNER_RADIUS,
            px("--pane-radius") - px("--ring-w")
        );
    }

    #[test]
    fn uncaptured_right_click_is_menu() {
        assert_eq!(right_click_route(false, false), Route::Menu);
    }

    #[test]
    fn captured_right_click_forwards_press_and_release() {
        let menu_press = Cell::new(0);
        let route = right_click_route(true, false);
        assert_eq!(route, Route::Program);
        assert!(
            begin_press(&menu_press, Button::Right, route),
            "press forwards"
        );
        assert_eq!(menu_press.get(), 0);
        assert!(end_press(&menu_press, Button::Right), "release forwards");
    }

    #[test]
    fn ctrl_click_is_menu_even_when_captured() {
        assert_eq!(right_click_route(true, true), Route::Menu);
        assert_eq!(right_click_route(false, true), Route::Menu);
    }

    #[test]
    fn right_click_does_not_forward_when_menu_shown() {
        let menu_press = Cell::new(0);
        assert!(
            !begin_press(&menu_press, Button::Right, Route::Menu),
            "press swallowed"
        );
        assert!(!end_press(&menu_press, Button::Right), "release swallowed");
        // The pair is spent: the next press/release pair forwards normally.
        assert!(begin_press(&menu_press, Button::Right, Route::Program));
        assert!(end_press(&menu_press, Button::Right));
    }

    #[test]
    fn a_menu_right_click_while_left_is_held_forwards_the_left_release() {
        // Hold left, right-click for the menu; the menu eats the right release. The left
        // release must still reach libghostty, or it keeps dragging a selection.
        let menu_press = Cell::new(0);
        assert!(begin_press(&menu_press, Button::Left, Route::Program));
        assert!(!begin_press(&menu_press, Button::Right, Route::Menu));
        assert!(end_press(&menu_press, Button::Left));
    }

    #[test]
    fn a_press_after_an_unreleased_menu_press_forwards_its_release() {
        // The popped menu's tracking loop can eat the menu press's release; the next press
        // must not inherit the swallow, or its release never reaches libghostty.
        let menu_press = Cell::new(0);
        assert!(!begin_press(&menu_press, Button::Left, Route::Menu));
        assert!(begin_press(&menu_press, Button::Left, Route::Program));
        assert!(end_press(&menu_press, Button::Left));
    }

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
                                         // AppKit's function-key characters (private use) — arrows, F-keys, Home/End.
        assert!(!is_printable_text("\u{F700}")); // ↑
        assert!(!is_printable_text("\u{F701}")); // ↓
        assert!(!is_printable_text("\u{F704}")); // F1
        assert!(!is_printable_text("\u{F8FF}")); // top of the range
    }

    #[test]
    fn a_null_window_is_an_error_not_a_crash() {
        let spec = crate::surface::TabSpec {
            colour: None,
            id: "t".into(),
            title: "t".into(),
            dir: "/tmp".into(),
            shell: "sh".into(),
            startup: None,
            group: None,
            probe: None,
            kill: None,
            suspend: None,
            tree: false,
            tree_path: Vec::new(),
            split: None,
            server: None,
            resume: None,
        };
        let rect = crate::surface::PixelRect {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        assert!(matches!(
            super::GhosttySurface::new(std::ptr::null_mut(), rect, &spec),
            Err(crate::surface::SurfaceError::SurfaceCreateFailed)
        ));
    }

    #[test]
    fn an_unchanged_clipboard_image_spills_once() {
        let dir = tempfile::tempdir().unwrap();
        let spills = std::cell::Cell::new(0);
        let spill = |name: &str| {
            spills.set(spills.get() + 1);
            let p = dir.path().join(name);
            std::fs::write(&p, b"png").unwrap();
            Some(p.to_str().unwrap().to_owned())
        };
        let mut cache = None;
        let first = super::reuse_or_spill(&mut cache, 7, || spill("a.png")).unwrap();
        for _ in 0..5 {
            assert_eq!(
                super::reuse_or_spill(&mut cache, 7, || spill("b.png")).as_deref(),
                Some(first.as_str())
            );
        }
        assert_eq!(spills.get(), 1);
        // A new clipboard (changeCount moved) spills afresh.
        let second = super::reuse_or_spill(&mut cache, 8, || spill("c.png")).unwrap();
        assert_ne!(second, first);
        assert_eq!(spills.get(), 2);
        // A reaped file is rewritten rather than pasted as a dangling path.
        std::fs::remove_file(&second).unwrap();
        super::reuse_or_spill(&mut cache, 8, || spill("d.png")).unwrap();
        assert_eq!(spills.get(), 3);
    }
}
