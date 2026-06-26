mod ffi;
mod geometry;
mod surface;

#[cfg(target_os = "macos")]
use surface::{ghostty::GhosttySurface, PixelRect, TabSpec, TerminalSurface};

use geometry::WebRect;
use std::sync::Mutex;

/// Newtype wrapper that holds the single GhosttySurface in Tauri-managed state.
/// `GhosttySurface: Send` (see ghostty.rs module docs). `Mutex<T>: Sync` when
/// `T: Send`, so `SurfaceHolder: Send + Sync` without additional unsafe impls.
/// All access must be on the main/UI thread (Tauri commands run there).
#[cfg(target_os = "macos")]
struct SurfaceHolder(Mutex<Option<GhosttySurface>>);

#[derive(serde::Deserialize)]
struct RectArg {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn set_hole_rect(
    window: tauri::WebviewWindow,
    state: tauri::State<SurfaceHolder>,
    rect: RectArg,
) {
    let scale = window.scale_factor().unwrap_or(1.0);
    // inner_size is in physical pixels; divide by scale to get points.
    let size = window.inner_size().unwrap_or(tauri::PhysicalSize::new(900, 600));
    let view_h = size.height as f64 / scale;
    let view_rect = geometry::web_rect_to_view(
        WebRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        view_h,
        scale,
    );
    if let Some(s) = state.0.lock().unwrap().as_ref() {
        s.set_frame(view_rect);
    }
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn set_hole_rect(_window: tauri::WebviewWindow, _rect: RectArg) {}

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
        .invoke_handler(tauri::generate_handler![set_hole_rect])
        .setup(|_app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;

                let win = _app.get_webview_window("main").expect("main window");
                let ns_window = win.ns_window().expect("ns_window") as *mut std::os::raw::c_void;

                let spec = TabSpec {
                    id: "t0".into(),
                    title: "shell".into(),
                    dir: std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into())),
                    cmd: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
                };
                // Full-window rect; `set_hole_rect` IPC updates this on every resize.
                let rect = PixelRect { x: 0.0, y: 0.0, width: 900.0, height: 600.0 };

                let s = GhosttySurface::new(ns_window, rect, &spec).expect("surface");
                s.show();
                s.focus();
                _app.manage(SurfaceHolder(Mutex::new(Some(s))));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running warden");
}
