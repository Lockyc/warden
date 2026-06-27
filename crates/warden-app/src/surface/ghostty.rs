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

use super::{PixelRect, SurfaceError, TabSpec, TerminalSurface};
use crate::ffi;
use crate::geometry;

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

use objc2::rc::{Allocated, Retained};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSEvent, NSResponder, NSView, NSWindow};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

// --- AppKit modifier-flag bit masks (stable AppKit ABI values) ---
const NS_FLAG_CAPS: usize = 1 << 16;
const NS_FLAG_SHIFT: usize = 1 << 17;
const NS_FLAG_CONTROL: usize = 1 << 18;
const NS_FLAG_OPTION: usize = 1 << 19;
const NS_FLAG_COMMAND: usize = 1 << 20;

// --- Process-global state for the single spike surface ---------------------
// The shared ghostty app handle (created once). Stored as usize so the static
// is trivially Send/Sync; reconstituted to a pointer on read.
static GHOSTTY_APP: OnceLock<usize> = OnceLock::new();
// The one live surface, so the host view's keyDown handler can forward keys
// without threading a pointer through ivars. Single-surface spike only.
static SURFACE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

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

/// App/surface actions (set-title, new-window, ring-bell, ...). For a single
/// embedded surface we handle none; returning false = "not handled", which the
/// reference (`Ghostty.App.swift`) also does for every unimplemented action.
unsafe extern "C" fn action_cb(
    _app: ffi::ghostty_app_t,
    _target: ffi::ghostty_target_s,
    _action: ffi::ghostty_action_s,
) -> bool {
    false
}

/// No clipboard integration in the spike: report "no data available".
unsafe extern "C" fn read_clipboard_cb(
    _userdata: *mut c_void,
    _loc: ffi::ghostty_clipboard_e,
    _state: *mut c_void,
) -> bool {
    false
}
unsafe extern "C" fn confirm_read_clipboard_cb(
    _userdata: *mut c_void,
    _str: *const c_char,
    _state: *mut c_void,
    _request: ffi::ghostty_clipboard_request_e,
) {
}
unsafe extern "C" fn write_clipboard_cb(
    _userdata: *mut c_void,
    _loc: ffi::ghostty_clipboard_e,
    _content: *const ffi::ghostty_clipboard_content_s,
    _len: usize,
    _confirm: bool,
) {
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
        type Ivars = ();
    }

    unsafe impl WardenHostView {
        #[method_id(initWithFrame:)]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> Option<Retained<Self>> {
            let this = this.set_ivars(());
            unsafe { msg_send_id![super(this), initWithFrame: frame] }
        }

        // Must be true for the view to accept key events as first responder.
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            unsafe { forward_key(event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_PRESS) };
        }

        #[method(keyUp:)]
        fn key_up(&self, event: &NSEvent) {
            unsafe { forward_key(event, ffi::ghostty_input_action_e::GHOSTTY_ACTION_RELEASE) };
        }
    }
);

/// Translate an AppKit key event into `ghostty_input_key_s` and forward it.
/// Minimal translation: `text` (from `characters`) carries printable input,
/// `keycode` is the macOS virtual keycode, `unshifted_codepoint` from
/// `charactersIgnoringModifiers`. Full IME / NSTextInputClient handling (dead
/// keys, marked text) is out of scope for the spike.
unsafe fn forward_key(event: &NSEvent, action: ffi::ghostty_input_action_e) {
    let surface = SURFACE.load(Ordering::Acquire);
    if surface.is_null() {
        return;
    }

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

    // `characters` is the resolved text for this keypress (already shaped by mods).
    let text = event.characters().map(|s| s.to_string());
    let c_text = text.as_deref().and_then(|s| CString::new(s).ok());
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
    ffi::ghostty_surface_key(surface, key);
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
        let mtm = MainThreadMarker::new()
            .expect("GhosttySurface::new must be called on the main thread");

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

            // Build the surface config from defaults, then override platform/dir/cmd.
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
            let c_cmd =
                CString::new(spec.cmd.clone()).map_err(|_| SurfaceError::SurfaceCreateFailed)?;
            cfg.working_directory = c_dir.as_ptr();
            cfg.command = c_cmd.as_ptr();

            let surface = ffi::ghostty_surface_new(app, &cfg);
            if surface.is_null() {
                host_view.removeFromSuperview();
                return Err(SurfaceError::SurfaceCreateFailed);
            }
            // NOTE: do NOT set the SURFACE key-routing global here. `focus()` is the
            // sole writer — routing follows the focused surface. Storing on creation
            // would steal key routing to an unfocused surface when a tab is created
            // after activation (e.g. a dynamic "new tab"), re-introducing a routing bug.

            ffi::ghostty_surface_set_content_scale(surface, scale, scale);
            let (w, h) = geometry::backing_size(rect, scale);
            ffi::ghostty_surface_set_size(surface, w, h);

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
            let (w, h) = geometry::backing_size(rect, scale);
            ffi::ghostty_surface_set_size(self.surface, w, h);
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
        // Track which surface is focused so keyDown: forwards to the active tab.
        SURFACE.store(self.surface, Ordering::Release);
        unsafe {
            ffi::ghostty_surface_set_focus(self.surface, true);
            ffi::ghostty_app_set_focus(shared_app(), true);
        }
    }

    fn close(self) {
        unsafe {
            // Only null the global if it still points at this surface —
            // closing a non-active surface must not blank the active one.
            let _ = SURFACE.compare_exchange(
                self.surface,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            ffi::ghostty_surface_free(self.surface);
            self.host_view.removeFromSuperview();
        }
    }
}
