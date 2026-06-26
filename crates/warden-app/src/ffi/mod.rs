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

// --- ghostty_runtime_config_s (opaque pointer target; Task 3 will flesh this out) ---
// Full struct is complex (carries multiple callback fn ptrs referencing action/clipboard types).
// Only used as *const in ghostty_app_new — a ZST opaque placeholder is sufficient here.
#[repr(C)]
pub struct ghostty_runtime_config_s {
    _opaque: [u8; 0],
}

// --- Published C API (minimal: init/app, surface new/free, set_size, set_content_scale, key, focus) ---
extern "C" {
    // int ghostty_init(uintptr_t, char**);
    pub fn ghostty_init(argc: usize, argv: *mut *mut c_char) -> c_int;

    // ghostty_app_t ghostty_app_new(const ghostty_runtime_config_s*, ghostty_config_t);
    pub fn ghostty_app_new(
        runtime_config: *const ghostty_runtime_config_s,
        config: ghostty_config_t,
    ) -> ghostty_app_t;

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
}
