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

// --- ghostty_surface_config_s ---
// ghostty_surface_config_new() returns this by value; layout must match the C struct exactly.
// C struct layout (arm64/x86_64 macOS):
//   offset  0: platform_tag (int32)
//   offset  8: platform (union, 8-byte aligned — 4 bytes padding after platform_tag)
//   offset 16: userdata (ptr)
//   offset 24: scale_factor (f64)
//   offset 32: font_size (f32)
//   offset 40: working_directory (ptr — 4 bytes padding after font_size)
//   offset 48: command (ptr)
//   offset 56: env_vars (ptr)
//   offset 64: env_var_count (usize)
//   offset 72: initial_input (ptr)
//   offset 80: wait_after_command (bool, 1 byte)
//   offset 84: context (int32 — 3 bytes padding after bool)
//   total: 88 bytes
//
// Ghostty main dropped the host-managed IO backend (the `backend`,
// `receive_userdata`, `receive_buffer`, `receive_resize` fields present through
// v1.3.1); warden always used the default EXEC backend, so this is a pure trim.
#[repr(C)]
pub struct ghostty_surface_config_s {
    pub platform_tag: ghostty_platform_e,
    pub platform: ghostty_platform_u,
    pub userdata: *mut c_void,
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
pub const GHOSTTY_ACTION_MOUSE_SHAPE: u32 = 36; // ghostty.h:911
pub const GHOSTTY_ACTION_RING_BELL: u32 = 50; // ghostty.h:925
pub const GHOSTTY_ACTION_OPEN_URL: u32 = 55; // ghostty.h:930
pub const GHOSTTY_ACTION_SHOW_CHILD_EXITED: u32 = 56; // ghostty.h:931

/// `ghostty_action_desktop_notification_s` (ghostty.h:650-653): the union variant for
/// `DESKTOP_NOTIFICATION`. Two borrowed C strings, valid only for the duration of the action_cb
/// call (libghostty owns them) — copy out before returning.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_action_desktop_notification_s {
    pub title: *const c_char,
    pub body: *const c_char,
}

/// `ghostty_action_open_url_s` (ghostty.h:818-822): the union variant for `OPEN_URL` — libghostty
/// asking the host to open a link the user clicked. `url` is **not NUL-terminated**: it is a
/// borrowed `(ptr, len)` slice owned by libghostty and valid only for this call, so copy it out.
/// `kind` is `ghostty_action_open_url_kind_e` (unknown/text/html); warden opens all kinds the same.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_action_open_url_s {
    pub kind: u32,
    pub url: *const c_char,
    pub len: usize,
}

/// `ghostty_surface_message_childexited_s` (ghostty.h:832-835): the union variant for
/// `SHOW_CHILD_EXITED` — the surface's child process is gone, so the terminal is dead.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ghostty_action_child_exited_s {
    pub exit_code: u32,
    pub runtime_ms: u64,
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

    /// The URL libghostty wants opened, copied out of the borrowed `(ptr, len)` slice. `None` for
    /// any other tag. Same cast-safety argument as `desktop_notification` (the tag proves the
    /// variant; the 24-byte struct exactly fills the 8-aligned union).
    pub fn open_url(&self) -> Option<String> {
        if self.tag != GHOSTTY_ACTION_OPEN_URL {
            return None;
        }
        let u = unsafe {
            &*(&self.action as *const ghostty_action_u as *const ghostty_action_open_url_s)
        };
        if u.url.is_null() || u.len == 0 {
            return None;
        }
        // NOT NUL-terminated — build from the explicit length, never `CStr`.
        let bytes = unsafe { std::slice::from_raw_parts(u.url as *const u8, u.len) };
        String::from_utf8(bytes.to_vec()).ok()
    }

    /// The child's exit code when the surface's process has died, else `None`.
    pub fn child_exited(&self) -> Option<u32> {
        if self.tag == GHOSTTY_ACTION_SHOW_CHILD_EXITED {
            let c = unsafe {
                &*(&self.action as *const ghostty_action_u as *const ghostty_action_child_exited_s)
            };
            Some(c.exit_code)
        } else {
            None
        }
    }

    /// The desired mouse-cursor shape (`ghostty_action_mouse_shape_e`, a bare C enum stored
    /// directly in the union), else `None`.
    pub fn mouse_shape(&self) -> Option<u32> {
        if self.tag == GHOSTTY_ACTION_MOUSE_SHAPE {
            Some(unsafe { *(&self.action as *const ghostty_action_u as *const u32) })
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
const _: () = assert!(std::mem::size_of::<ghostty_surface_config_s>() == 88);
const _: () = assert!(std::mem::size_of::<ghostty_runtime_config_s>() == 64);
const _: () = assert!(std::mem::size_of::<ghostty_target_s>() == 16);
const _: () = assert!(std::mem::size_of::<ghostty_action_s>() == 32);
// Action union variants warden reads. Each must fit the 24-byte, 8-aligned union blob — a variant
// that outgrew it would read past the action struct. (Discriminants can't be guarded this way; see
// the tag consts above and CLAUDE.md's "eyeball the action tags on a version jump".)
const _: () = assert!(std::mem::size_of::<ghostty_action_desktop_notification_s>() <= 24);
const _: () = assert!(std::mem::size_of::<ghostty_action_open_url_s>() == 24);
const _: () = assert!(std::mem::size_of::<ghostty_action_child_exited_s>() <= 24);
// ghostty_input_key_s is passed by value to forward_key's ghostty_surface_key — a header
// bump that shifts `text`/`unshifted_codepoint`'s offsets would silently corrupt every
// keystroke, so guard its size too (3×enum + keycode = 16, ptr 16..24, u32+bool → 32).
const _: () = assert!(std::mem::size_of::<ghostty_input_key_s>() == 32);

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

    // void ghostty_surface_text(ghostty_surface_t, const char*, uintptr_t);
    // Inject text into the surface as if typed — the runtime equivalent of the config's
    // `initial_input`. Length-delimited (not a C string), so embedded NULs are fine.
    pub fn ghostty_surface_text(surface: ghostty_surface_t, text: *const c_char, len: usize);

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
        let title = CString::new("Claude — alpha").unwrap();
        let body = CString::new("waiting for permission").unwrap();
        let action = notification_action(&ghostty_action_desktop_notification_s {
            title: title.as_ptr(),
            body: body.as_ptr(),
        });
        let dn = action.desktop_notification().expect("tag matches → Some");
        // SAFETY: pointers reference the live CStrings above.
        assert_eq!(
            unsafe { CStr::from_ptr(dn.title) }.to_str().unwrap(),
            "Claude — alpha"
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
        assert!(other.open_url().is_none());
        assert!(other.child_exited().is_none());
        assert!(other.mouse_shape().is_none());
    }

    /// Write a variant into the union prefix, exactly as libghostty lays it out.
    /// SAFETY (all three uses): the variant fits the 8-aligned 24-byte union (const-asserted above).
    fn action_with<T>(tag: u32, payload: T) -> ghostty_action_s {
        let mut a = ghostty_action_s {
            tag,
            action: ghostty_action_u { _bytes: [0; 24] },
        };
        unsafe { std::ptr::write(&mut a.action as *mut ghostty_action_u as *mut T, payload) };
        a
    }

    #[test]
    fn decodes_open_url_from_a_non_nul_terminated_slice() {
        // libghostty hands over (ptr, len) into a buffer that is NOT NUL-terminated — reading it as
        // a CStr would run past the end. The trailing junk here would be picked up by that bug.
        let backing = b"https://example.com/x?a=1JUNKJUNK";
        let action = action_with(
            GHOSTTY_ACTION_OPEN_URL,
            ghostty_action_open_url_s {
                kind: 1, // TEXT
                url: backing.as_ptr() as *const c_char,
                len: "https://example.com/x?a=1".len(),
            },
        );
        assert_eq!(
            action.open_url().as_deref(),
            Some("https://example.com/x?a=1")
        );
        // …and it must not be mistaken for anything else.
        assert!(!action.is_ring_bell());
        assert!(action.desktop_notification().is_none());
    }

    #[test]
    fn open_url_with_a_null_or_empty_url_is_dropped() {
        let null = action_with(
            GHOSTTY_ACTION_OPEN_URL,
            ghostty_action_open_url_s {
                kind: 0,
                url: std::ptr::null(),
                len: 7,
            },
        );
        assert!(null.open_url().is_none());
        let empty = action_with(
            GHOSTTY_ACTION_OPEN_URL,
            ghostty_action_open_url_s {
                kind: 0,
                url: b"x".as_ptr() as *const c_char,
                len: 0,
            },
        );
        assert!(empty.open_url().is_none());
    }

    #[test]
    fn decodes_child_exited_exit_code() {
        let action = action_with(
            GHOSTTY_ACTION_SHOW_CHILD_EXITED,
            ghostty_action_child_exited_s {
                exit_code: 130, // e.g. SIGINT
                runtime_ms: 4_200,
            },
        );
        assert_eq!(action.child_exited(), Some(130));
        assert!(action.open_url().is_none());
    }

    #[test]
    fn decodes_mouse_shape_enum() {
        // The union holds the bare C enum; 3 = POINTER (a link is under the cursor).
        let action = action_with(GHOSTTY_ACTION_MOUSE_SHAPE, 3u32);
        assert_eq!(action.mouse_shape(), Some(3));
        assert!(action.child_exited().is_none());
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
