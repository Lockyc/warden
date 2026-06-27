mod ffi;
mod geometry;
mod surface;

#[cfg(not(target_os = "macos"))]
compile_error!("warden-app currently targets macOS only (libghostty surface embed). Linux is a later spike.");

#[cfg(target_os = "macos")]
mod registry;

#[cfg(target_os = "macos")]
use registry::Registry;

#[cfg(target_os = "macos")]
use surface::{PixelRect, TabSpec};

use geometry::WebRect;
use std::sync::Mutex;

/// Holds the surface registry in Tauri-managed state.
/// `Registry` contains `GhosttySurface: Send` values behind a `Mutex`; all
/// access must be on the main/UI thread (Tauri commands run there).
#[cfg(target_os = "macos")]
struct AppState(Mutex<Registry>);

#[derive(serde::Serialize, Clone)]
struct TabSpecDto {
    id: String,
    title: String,
    colour: String,
}

#[derive(serde::Deserialize)]
struct RectArg {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Hardcoded tab specs for the spike. Task 6 reads these from a config Profile.
#[cfg(target_os = "macos")]
fn specs() -> Vec<TabSpec> {
    let home = std::env::var("HOME").unwrap_or("/".into());
    vec![
        TabSpec {
            id: "t0".into(),
            title: "home".into(),
            dir: home.into(),
            cmd: "fish".into(),
        },
        TabSpec {
            id: "t1".into(),
            title: "tmp".into(),
            dir: "/tmp".into(),
            cmd: "bash".into(),
        },
        TabSpec {
            id: "t2".into(),
            title: "root".into(),
            dir: "/".into(),
            cmd: "bash".into(),
        },
    ]
}

/// Return tab descriptors so the web chrome can build the tab bar.
/// Banner colour is hardcoded for the spike (Plan 2 sources it from Profile.colour).
#[cfg(target_os = "macos")]
#[tauri::command]
fn init_tabs() -> Vec<TabSpecDto> {
    specs()
        .into_iter()
        .map(|s| TabSpecDto { id: s.id, title: s.title, colour: "#0f8a8a".into() })
        .collect()
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn activate_tab(state: tauri::State<AppState>, id: String) {
    state.0.lock().unwrap().activate(&id);
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn set_hole_rect(
    window: tauri::WebviewWindow,
    state: tauri::State<AppState>,
    rect: RectArg,
) {
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
    let size = window.inner_size().expect("main window inner_size");
    let view_h = size.height as f64 / scale;
    let view_rect = geometry::web_rect_to_view(
        WebRect { x, y, width, height },
        view_h,
    );
    state.0.lock().unwrap().set_active_frame(view_rect);
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
        .setup(|_app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;

                let win = _app.get_webview_window("main").expect("main window");
                let ns_window = win.ns_window().expect("ns_window") as *mut std::os::raw::c_void;

                let tab_specs = specs();
                // Initial rect: offset by sidebar width (160 px) so the surface never
                // overlaps the sidebar — even on the first frame before the JS ResizeObserver
                // IPC arrives to correct the geometry.
                let rect = PixelRect { x: 160.0, y: 0.0, width: 740.0, height: 600.0 };

                let mut registry = Registry::new();
                // Eager creation: spawn all 3 shell surfaces at startup (all hidden).
                // They share the one libghostty app handle; switching just shows/hides.
                for spec in &tab_specs {
                    registry.create(ns_window, rect, spec);
                }
                registry.activate("t0");

                _app.manage(AppState(Mutex::new(registry)));

                // Teardown: free every surface and reap its shell when the window is closed.
                let handle = _app.handle().clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::Destroyed = event {
                        if let Some(state) = handle.try_state::<AppState>() {
                            state.0.lock().unwrap().close_all();
                        }
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running warden");
}
