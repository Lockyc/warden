//! Hand-written bindings to the libghostty embedding C API, transcribed from
//! vendor/ghostty.h at the pinned commit. The API is officially unstable —
//! keep this module minimal and isolated; nothing else in the crate calls C.
#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_int, c_void};

// --- Opaque handles — typedef void* in the header ---
pub type ghostty_app_t = *mut c_void;
pub type ghostty_config_t = *mut c_void;
pub type ghostty_surface_t = *mut c_void;

// --- ghostty_platform_e ---
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_platform_e {
    GHOSTTY_PLATFORM_INVALID = 0,
    GHOSTTY_PLATFORM_MACOS = 1,
    GHOSTTY_PLATFORM_IOS = 2,
}

// --- ghostty_surface_io_backend_e ---
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_surface_io_backend_e {
    GHOSTTY_SURFACE_IO_BACKEND_EXEC = 0,
    GHOSTTY_SURFACE_IO_BACKEND_HOST_MANAGED = 1,
}

// --- ghostty_surface_context_e ---
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_surface_context_e {
    GHOSTTY_SURFACE_CONTEXT_WINDOW = 0,
    GHOSTTY_SURFACE_CONTEXT_TAB = 1,
    GHOSTTY_SURFACE_CONTEXT_SPLIT = 2,
}

// --- ghostty_input_action_e ---
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_input_action_e {
    GHOSTTY_ACTION_RELEASE = 0,
    GHOSTTY_ACTION_PRESS = 1,
    GHOSTTY_ACTION_REPEAT = 2,
}

// --- ghostty_input_mods_e (bit flags — kept as c_int to allow ORed values) ---
pub type ghostty_input_mods_e = c_int;
pub const GHOSTTY_MODS_NONE: ghostty_input_mods_e = 0;
pub const GHOSTTY_MODS_SHIFT: ghostty_input_mods_e = 1 << 0;
pub const GHOSTTY_MODS_CTRL: ghostty_input_mods_e = 1 << 1;
pub const GHOSTTY_MODS_ALT: ghostty_input_mods_e = 1 << 2;
pub const GHOSTTY_MODS_SUPER: ghostty_input_mods_e = 1 << 3;
pub const GHOSTTY_MODS_CAPS: ghostty_input_mods_e = 1 << 4;

// --- ghostty_input_mouse_state_e ---
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_input_mouse_state_e {
    GHOSTTY_MOUSE_RELEASE = 0,
    GHOSTTY_MOUSE_PRESS = 1,
}

// --- ghostty_input_mouse_button_e (we forward left/right/middle; the rest map to UNKNOWN) ---
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_input_mouse_button_e {
    GHOSTTY_MOUSE_UNKNOWN = 0,
    GHOSTTY_MOUSE_LEFT = 1,
    GHOSTTY_MOUSE_RIGHT = 2,
    GHOSTTY_MOUSE_MIDDLE = 3,
}

// --- ghostty_input_scroll_mods_t (typedef int: bit 0 = precision deltas, bits 1-3 = momentum) ---
pub type ghostty_input_scroll_mods_t = c_int;

// --- Platform handle struct (carries NSView* on macOS) ---
// Transcribed from: typedef struct { void* nsview; } ghostty_platform_macos_s;
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_platform_macos_s {
    pub nsview: *mut c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_platform_ios_s {
    pub uiview: *mut c_void,
}

// typedef union { ghostty_platform_macos_s macos; ghostty_platform_ios_s ios; } ghostty_platform_u;
#[repr(C)]
pub union ghostty_platform_u {
    pub macos: ghostty_platform_macos_s,
    pub ios: ghostty_platform_ios_s,
}

// --- ghostty_env_var_s ---
#[repr(C)]
pub struct ghostty_env_var_s {
    pub key: *const c_char,
    pub value: *const c_char,
}

// --- Callback types embedded in ghostty_surface_config_s ---
// typedef void (*ghostty_surface_receive_buffer_cb)(void*, const uint8_t*, size_t);
pub type ghostty_surface_receive_buffer_cb =
    Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)>;

// typedef void (*ghostty_surface_receive_resize_cb)(void*, uint16_t, uint16_t, uint32_t, uint32_t);
pub type ghostty_surface_receive_resize_cb =
    Option<unsafe extern "C" fn(*mut c_void, u16, u16, u32, u32)>;

// --- ghostty_surface_config_s ---
// ghostty_surface_config_new() returns this by value; layout must match the C struct exactly.
// C struct layout (arm64/x86_64 macOS):
//   offset  0: platform_tag (int32)
//   offset  8: platform (union, 8-byte aligned — 4 bytes padding after platform_tag)
//   offset 16: userdata (ptr)
//   offset 24: backend (int32)
//   offset 32: receive_userdata (ptr — 4 bytes padding after backend)
//   offset 40: receive_buffer (fn ptr)
//   offset 48: receive_resize (fn ptr)
//   offset 56: scale_factor (f64)
//   offset 64: font_size (f32)
//   offset 72: working_directory (ptr — 4 bytes padding after font_size)
//   offset 80: command (ptr)
//   offset 88: env_vars (ptr)
//   offset 96: env_var_count (usize)
//   offset 104: initial_input (ptr)
//   offset 112: wait_after_command (bool, 1 byte)
//   offset 116: context (int32 — 3 bytes padding after bool)
//   total: 120 bytes
#[repr(C)]
pub struct ghostty_surface_config_s {
    pub platform_tag: ghostty_platform_e,
    pub platform: ghostty_platform_u,
    pub userdata: *mut c_void,
    pub backend: ghostty_surface_io_backend_e,
    pub receive_userdata: *mut c_void,
    pub receive_buffer: ghostty_surface_receive_buffer_cb,
    pub receive_resize: ghostty_surface_receive_resize_cb,
    pub scale_factor: f64,
    pub font_size: f32,
    pub working_directory: *const c_char,
    pub command: *const c_char,
    pub env_vars: *mut ghostty_env_var_s,
    pub env_var_count: usize,
    pub initial_input: *const c_char,
    pub wait_after_command: bool,
    pub context: ghostty_surface_context_e,
}

// --- ghostty_input_key_s ---
// typedef struct { action, mods, consumed_mods, keycode, text, unshifted_codepoint, composing }
#[repr(C)]
pub struct ghostty_input_key_s {
    pub action: ghostty_input_action_e,
    pub mods: ghostty_input_mods_e,
    pub consumed_mods: ghostty_input_mods_e,
    pub keycode: u32,
    pub text: *const c_char,
    pub unshifted_codepoint: u32,
    pub composing: bool,
}

// --- Clipboard enums/structs (referenced by runtime callbacks) ---
// typedef enum { GHOSTTY_CLIPBOARD_STANDARD, GHOSTTY_CLIPBOARD_SELECTION } ghostty_clipboard_e;
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_clipboard_e {
    GHOSTTY_CLIPBOARD_STANDARD = 0,
    GHOSTTY_CLIPBOARD_SELECTION = 1,
}

// typedef enum { PASTE, OSC_52_READ, OSC_52_WRITE } ghostty_clipboard_request_e;
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ghostty_clipboard_request_e {
    GHOSTTY_CLIPBOARD_REQUEST_PASTE = 0,
    GHOSTTY_CLIPBOARD_REQUEST_OSC_52_READ = 1,
    GHOSTTY_CLIPBOARD_REQUEST_OSC_52_WRITE = 2,
}

// typedef struct { const char* mime; const char* data; } ghostty_clipboard_content_s;
#[repr(C)]
pub struct ghostty_clipboard_content_s {
    pub mime: *const c_char,
    pub data: *const c_char,
}

// --- ghostty_target_s (passed BY VALUE to action_cb; 16 bytes, verified via clang) ---
// typedef union { ghostty_surface_t surface; } ghostty_target_u;
#[repr(C)]
#[derive(Copy, Clone)]
pub union ghostty_target_u {
    pub surface: ghostty_surface_t,
}
// typedef struct { ghostty_target_tag_e tag; ghostty_target_u target; } ghostty_target_s;
// tag is a C enum (4 bytes); kept as u32 so the 16-byte layout matches exactly.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_target_s {
    pub tag: u32,
    pub target: ghostty_target_u,
}

// --- ghostty_action_s (passed BY VALUE to action_cb; 32 bytes total, verified via clang) ---
// The real `action` member is a large tagged union (24 bytes, many variants). Because the
// whole struct is 32 bytes (>16), the AArch64 / SysV-x86_64 C ABI passes it INDIRECTLY (by
// hidden pointer). We model the union as an opaque, correctly-aligned 24-byte blob and read
// only the variants warden acts on, reinterpreting the blob per the `tag` (see methods below).
#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct ghostty_action_u {
    _bytes: [u8; 24],
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_action_s {
    pub tag: u32,
    pub action: ghostty_action_u,
}

// Action tag discriminants we handle. `ghostty_action_tag_e` is a plain C enum, sequential from
// `GHOSTTY_ACTION_QUIT = 0` (vendored ghostty.h:875) with no explicit values, so each value equals
// its 0-based position. Read `tag` as a u32 and COMPARE (never transmute into a Rust enum) — an
// unknown value from a future libghostty is then just "unhandled", not invalid-discriminant UB.
pub const GHOSTTY_ACTION_DESKTOP_NOTIFICATION: u32 = 31; // ghostty.h:906
pub const GHOSTTY_ACTION_RING_BELL: u32 = 50; // ghostty.h:925

/// `ghostty_action_desktop_notification_s` (ghostty.h:650-653): the union variant for
/// `DESKTOP_NOTIFICATION`. Two borrowed C strings, valid only for the duration of the action_cb
/// call (libghostty owns them) — copy out before returning.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_action_desktop_notification_s {
    pub title: *const c_char,
    pub body: *const c_char,
}

impl ghostty_action_s {
    pub fn is_ring_bell(&self) -> bool {
        self.tag == GHOSTTY_ACTION_RING_BELL
    }
    /// Reinterpret the union as the desktop-notification payload, but only when the tag says so.
    /// SAFETY of the cast: the tag guarantees the union holds this variant, and the 16-byte
    /// struct is a prefix of the 8-aligned 24-byte union, so the read is in-bounds and aligned.
    pub fn desktop_notification(&self) -> Option<&ghostty_action_desktop_notification_s> {
        if self.tag == GHOSTTY_ACTION_DESKTOP_NOTIFICATION {
            Some(unsafe {
                &*(&self.action as *const ghostty_action_u
                    as *const ghostty_action_desktop_notification_s)
            })
        } else {
            None
        }
    }
}

// Target tag: `GHOSTTY_TARGET_APP = 0`, `GHOSTTY_TARGET_SURFACE = 1` (ghostty.h:545-546).
pub const GHOSTTY_TARGET_SURFACE: u32 = 1;

impl ghostty_target_s {
    /// The surface this action targets, or `None` for app-level targets (no tab to route to).
    pub fn surface(&self) -> Option<ghostty_surface_t> {
        if self.tag == GHOSTTY_TARGET_SURFACE {
            // SAFETY: the union holds a surface pointer exactly when the tag is SURFACE.
            Some(unsafe { self.target.surface })
        } else {
            None
        }
    }
}

// --- Runtime callback function-pointer types (vendored header lines 988-1005) ---
// typedef void (*ghostty_runtime_wakeup_cb)(void*);
pub type ghostty_runtime_wakeup_cb = Option<unsafe extern "C" fn(*mut c_void)>;
// typedef bool (*ghostty_runtime_action_cb)(ghostty_app_t, ghostty_target_s, ghostty_action_s);
pub type ghostty_runtime_action_cb =
    Option<unsafe extern "C" fn(ghostty_app_t, ghostty_target_s, ghostty_action_s) -> bool>;
// typedef bool (*ghostty_runtime_read_clipboard_cb)(void*, ghostty_clipboard_e, void*);
pub type ghostty_runtime_read_clipboard_cb =
    Option<unsafe extern "C" fn(*mut c_void, ghostty_clipboard_e, *mut c_void) -> bool>;
// typedef void (*ghostty_runtime_confirm_read_clipboard_cb)(void*, const char*, void*, ghostty_clipboard_request_e);
pub type ghostty_runtime_confirm_read_clipboard_cb = Option<
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void, ghostty_clipboard_request_e),
>;
// typedef void (*ghostty_runtime_write_clipboard_cb)(void*, ghostty_clipboard_e, const ghostty_clipboard_content_s*, size_t, bool);
pub type ghostty_runtime_write_clipboard_cb = Option<
    unsafe extern "C" fn(
        *mut c_void,
        ghostty_clipboard_e,
        *const ghostty_clipboard_content_s,
        usize,
        bool,
    ),
>;
// typedef void (*ghostty_runtime_close_surface_cb)(void*, bool);
pub type ghostty_runtime_close_surface_cb = Option<unsafe extern "C" fn(*mut c_void, bool)>;

// --- ghostty_runtime_config_s (vendored header lines 1007-1016; 64 bytes, verified via clang) ---
#[repr(C)]
pub struct ghostty_runtime_config_s {
    pub userdata: *mut c_void,
    pub supports_selection_clipboard: bool,
    pub wakeup_cb: ghostty_runtime_wakeup_cb,
    pub action_cb: ghostty_runtime_action_cb,
    pub read_clipboard_cb: ghostty_runtime_read_clipboard_cb,
    pub confirm_read_clipboard_cb: ghostty_runtime_confirm_read_clipboard_cb,
    pub write_clipboard_cb: ghostty_runtime_write_clipboard_cb,
    pub close_surface_cb: ghostty_runtime_close_surface_cb,
}

// --- Header-drift guards: assert struct sizes match the vendored C header exactly.
// These are compile-time and break the build immediately if a future header bump shifts layout.
const _: () = assert!(std::mem::size_of::<ghostty_surface_config_s>() == 120);
const _: () = assert!(std::mem::size_of::<ghostty_runtime_config_s>() == 64);
const _: () = assert!(std::mem::size_of::<ghostty_target_s>() == 16);
const _: () = assert!(std::mem::size_of::<ghostty_action_s>() == 32);

// --- Published C API (minimal: init/app, surface new/free, set_size, set_content_scale, key, focus) ---
extern "C" {
    // int ghostty_init(uintptr_t, char**);
    pub fn ghostty_init(argc: usize, argv: *mut *mut c_char) -> c_int;

    // ghostty_config_t ghostty_config_new();
    pub fn ghostty_config_new() -> ghostty_config_t;
    // void ghostty_config_load_default_files(ghostty_config_t);
    pub fn ghostty_config_load_default_files(config: ghostty_config_t);
    // void ghostty_config_finalize(ghostty_config_t);
    pub fn ghostty_config_finalize(config: ghostty_config_t);
    // void ghostty_config_free(ghostty_config_t);
    pub fn ghostty_config_free(config: ghostty_config_t);

    // ghostty_app_t ghostty_app_new(const ghostty_runtime_config_s*, ghostty_config_t);
    pub fn ghostty_app_new(
        runtime_config: *const ghostty_runtime_config_s,
        config: ghostty_config_t,
    ) -> ghostty_app_t;

    // void ghostty_app_free(ghostty_app_t);
    pub fn ghostty_app_free(app: ghostty_app_t);

    // void ghostty_app_tick(ghostty_app_t);
    pub fn ghostty_app_tick(app: ghostty_app_t);

    // void ghostty_app_set_focus(ghostty_app_t, bool);
    pub fn ghostty_app_set_focus(app: ghostty_app_t, focused: bool);

    // ghostty_surface_config_s ghostty_surface_config_new();
    pub fn ghostty_surface_config_new() -> ghostty_surface_config_s;

    // ghostty_surface_t ghostty_surface_new(ghostty_app_t, const ghostty_surface_config_s*);
    pub fn ghostty_surface_new(
        app: ghostty_app_t,
        config: *const ghostty_surface_config_s,
    ) -> ghostty_surface_t;

    // void ghostty_surface_free(ghostty_surface_t);
    pub fn ghostty_surface_free(surface: ghostty_surface_t);

    // void ghostty_surface_set_size(ghostty_surface_t, uint32_t, uint32_t);
    pub fn ghostty_surface_set_size(surface: ghostty_surface_t, width: u32, height: u32);

    // void ghostty_surface_set_content_scale(ghostty_surface_t, double, double);
    pub fn ghostty_surface_set_content_scale(surface: ghostty_surface_t, x: f64, y: f64);

    // bool ghostty_surface_key(ghostty_surface_t, ghostty_input_key_s);
    pub fn ghostty_surface_key(surface: ghostty_surface_t, key: ghostty_input_key_s) -> bool;

    // void ghostty_surface_set_focus(ghostty_surface_t, bool);
    pub fn ghostty_surface_set_focus(surface: ghostty_surface_t, focused: bool);

    // bool ghostty_surface_mouse_button(ghostty_surface_t, state_e, button_e, mods_e);
    pub fn ghostty_surface_mouse_button(
        surface: ghostty_surface_t,
        state: ghostty_input_mouse_state_e,
        button: ghostty_input_mouse_button_e,
        mods: ghostty_input_mods_e,
    ) -> bool;

    // void ghostty_surface_mouse_pos(ghostty_surface_t, double, double, mods_e);
    pub fn ghostty_surface_mouse_pos(
        surface: ghostty_surface_t,
        x: f64,
        y: f64,
        mods: ghostty_input_mods_e,
    );

    // void ghostty_surface_mouse_scroll(ghostty_surface_t, double, double, scroll_mods_t);
    pub fn ghostty_surface_mouse_scroll(
        surface: ghostty_surface_t,
        dx: f64,
        dy: f64,
        mods: ghostty_input_scroll_mods_t,
    );

    // void ghostty_surface_complete_clipboard_request(ghostty_surface_t, const char*, void*, bool);
    // Hands clipboard data back to libghostty in response to a read_clipboard_cb; `state` is the
    // opaque request token from that callback, `confirmed` skips the unsafe-paste confirmation.
    pub fn ghostty_surface_complete_clipboard_request(
        surface: ghostty_surface_t,
        data: *const c_char,
        state: *mut c_void,
        confirmed: bool,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    /// Build an action whose union prefix holds a desktop-notification payload, exactly as
    /// libghostty would (tag at offset 0, the two C-string pointers at offset 8).
    fn notification_action(n: &ghostty_action_desktop_notification_s) -> ghostty_action_s {
        let mut a = ghostty_action_s {
            tag: GHOSTTY_ACTION_DESKTOP_NOTIFICATION,
            action: ghostty_action_u { _bytes: [0; 24] },
        };
        // SAFETY: writing the 16-byte variant into the 8-aligned 24-byte union prefix.
        unsafe {
            std::ptr::write(
                &mut a.action as *mut ghostty_action_u
                    as *mut ghostty_action_desktop_notification_s,
                *n,
            );
        }
        a
    }

    #[test]
    fn decodes_desktop_notification_title_and_body() {
        let title = CString::new("Claude — locus").unwrap();
        let body = CString::new("waiting for permission").unwrap();
        let action = notification_action(&ghostty_action_desktop_notification_s {
            title: title.as_ptr(),
            body: body.as_ptr(),
        });
        let dn = action.desktop_notification().expect("tag matches → Some");
        // SAFETY: pointers reference the live CStrings above.
        assert_eq!(
            unsafe { CStr::from_ptr(dn.title) }.to_str().unwrap(),
            "Claude — locus"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(dn.body) }.to_str().unwrap(),
            "waiting for permission"
        );
        assert!(!action.is_ring_bell());
    }

    #[test]
    fn ring_bell_tag_is_recognised_and_not_a_notification() {
        let bell = ghostty_action_s {
            tag: GHOSTTY_ACTION_RING_BELL,
            action: ghostty_action_u { _bytes: [0; 24] },
        };
        assert!(bell.is_ring_bell());
        assert!(bell.desktop_notification().is_none());
    }

    #[test]
    fn unknown_action_tag_decodes_to_nothing() {
        // A tag warden doesn't handle (e.g. a future libghostty value) is inert, never UB.
        let other = ghostty_action_s {
            tag: 9999,
            action: ghostty_action_u { _bytes: [0; 24] },
        };
        assert!(!other.is_ring_bell());
        assert!(other.desktop_notification().is_none());
    }

    #[test]
    fn target_surface_is_extracted_only_for_surface_tag() {
        let mut ptr = 0u8;
        let surface = &mut ptr as *mut u8 as ghostty_surface_t;
        let surf_target = ghostty_target_s {
            tag: GHOSTTY_TARGET_SURFACE,
            target: ghostty_target_u { surface },
        };
        assert_eq!(surf_target.surface(), Some(surface));
        // App-targeted (tag 0) → no surface to route to.
        let app_target = ghostty_target_s {
            tag: 0,
            target: ghostty_target_u { surface },
        };
        assert_eq!(app_target.surface(), None);
    }
}
