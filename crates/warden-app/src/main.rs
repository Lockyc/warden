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

/// Holds the config-file watcher for the app's lifetime. The watcher stops
/// firing the moment it is dropped, so it must live in managed state rather
/// than as a local in `setup`. [seam: manager only]
#[cfg(target_os = "macos")]
struct WatcherState(#[allow(dead_code)] warden_config::Watcher);

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

                // Hot-reload: watch the config file; on each event reload + diff
                // against last_good + apply the resulting WindowOps to live
                // windows. The notify callback runs on a background thread, but
                // every Tauri/AppKit/registry touch is main-thread only — hop via
                // run_on_main_thread before doing any of it.
                let cfg_path = warden_config::config_path();
                // Watcher::new requires the config's parent dir to already exist.
                if let Some(parent) = cfg_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let wh = app.handle().clone();
                let watcher = warden_config::Watcher::new(cfg_path, move |res| {
                    let wh = wh.clone();
                    let _ = wh.clone().run_on_main_thread(move || {
                        use tauri::{Emitter, Manager};
                        match res {
                            Ok(loaded) => {
                                let st = wh.state::<ManagerState>();
                                let mut m = st.0.lock().unwrap();
                                let recon =
                                    warden_config::reconcile(&m.last_good, &loaded.config);
                                m.apply(&wh, &recon);
                                // Advance the reconcile baseline ONLY on a valid load.
                                m.last_good = loaded.config.clone();
                                // Clear any stale error banner (Task 8 renders it).
                                let _ = wh.emit("warden:error-clear", ());
                            }
                            Err(e) => {
                                // Keep last_good; surface the error (Task 8 renders it).
                                let _ = wh.emit("warden:error", e.to_string());
                            }
                        }
                    });
                });
                // Keep the watcher alive for the app's lifetime. Log a failure so a
                // dead watcher (no hot-reload) is distinguishable from a working one.
                match watcher {
                    Ok(w) => {
                        app.manage(WatcherState(w));
                    }
                    Err(e) => {
                        eprintln!("warden: failed to start config watcher (no hot-reload): {e}");
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running warden");
}
