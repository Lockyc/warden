mod ffi;
mod surface;

#[cfg(target_os = "macos")]
use surface::{ghostty::GhosttySurface, PixelRect, TabSpec, TerminalSurface};

fn main() {
    // libghostty must be initialised once before any app/surface is created.
    // (Checkpoint 0 smoke test; non-zero return is logged but non-fatal here.)
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
                // Full-window rect (Task 4 will replace this with the reported hole rect).
                let rect = PixelRect { x: 0.0, y: 0.0, width: 900.0, height: 600.0 };

                let s = GhosttySurface::new(ns_window, rect, &spec).expect("surface");
                s.show();
                s.focus();
                // Keep it alive for the session (main-thread access only; see module docs).
                _app.manage(std::sync::Mutex::new(Some(s)));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running warden");
}
