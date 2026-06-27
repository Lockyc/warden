mod ffi;
mod geometry;
mod plan;
mod surface;

#[cfg(not(target_os = "macos"))]
compile_error!("warden-app currently targets macOS only (libghostty surface embed). Linux is a later spike.");

#[cfg(target_os = "macos")]
mod manager;

#[cfg(target_os = "macos")]
mod registry;

#[cfg(target_os = "macos")]
use manager::{InitDto, WindowManager};

use geometry::WebRect;

#[derive(serde::Deserialize)]
struct RectArg {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// All live profile windows, behind a `Mutex`. Each `WindowState` holds a
/// `Registry` with `GhosttySurface: Send` values; all access is on the
/// main/UI thread (Tauri commands run there). [seam: manager only]
#[cfg(target_os = "macos")]
struct ManagerState(std::sync::Mutex<WindowManager>);

/// Return the calling window's banner + tab descriptors, resolved by label.
#[cfg(target_os = "macos")]
#[tauri::command]
fn init_tabs(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) -> Option<InitDto> {
    state.0.lock().unwrap().init_dto(window.label())
}

/// Activate tab `id` within the calling window's registry.
#[cfg(target_os = "macos")]
#[tauri::command]
fn activate_tab(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    let mut m = state.0.lock().unwrap();
    if let Some(ws) = m.windows.get_mut(window.label()) {
        ws.registry.activate(&id);
    }
}

/// Update the calling window's active-surface frame from a web-coordinate rect.
#[cfg(target_os = "macos")]
#[tauri::command]
fn set_hole_rect(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, rect: RectArg) {
    // Reject non-finite values before they reach NSView or libghostty.
    if !rect.x.is_finite() || !rect.y.is_finite() || !rect.width.is_finite() || !rect.height.is_finite() {
        return;
    }
    // Clamp to sane bounds: huge values saturate u32 in ghostty_surface_set_size.
    let x = rect.x.clamp(-100_000.0, 100_000.0);
    let y = rect.y.clamp(-100_000.0, 100_000.0);
    let width = rect.width.clamp(0.0, 100_000.0);
    let height = rect.height.clamp(0.0, 100_000.0);

    let scale = window.scale_factor().unwrap_or(1.0);
    // inner_size is in physical pixels; divide by scale to get points.
    let size = window.inner_size().expect("inner_size");
    let view_h = size.height as f64 / scale;
    let view_rect = geometry::web_rect_to_view(WebRect { x, y, width, height }, view_h);

    let mut m = state.0.lock().unwrap();
    if let Some(ws) = m.windows.get_mut(window.label()) {
        ws.registry.set_active_frame(view_rect);
    }
}

fn main() {
    // libghostty must be initialised once before any app/surface is created.
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::raw::c_char;

        let args: Vec<CString> = std::env::args()
            .map(|a| CString::new(a).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let mut c_argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
        c_argv.push(std::ptr::null_mut());

        let ret = unsafe { ffi::ghostty_init(args.len(), c_argv.as_mut_ptr()) };
        if ret != 0 {
            eprintln!("warden: ghostty_init returned {ret} (non-fatal)");
        }
    }

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![set_hole_rect, init_tabs, activate_tab])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;

                let handle = app.handle().clone();
                let mut mgr = WindowManager::new();
                // Load config; Task 8 adds the diagnostic-window fallback. For this
                // checkpoint, expect a valid config to exist.
                let loaded = warden_config::load(&warden_config::config_path())
                    .expect("load config (Task 8 adds graceful failure)");
                mgr.materialize(&handle, loaded.config);
                app.manage(ManagerState(std::sync::Mutex::new(mgr)));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running warden");
}
