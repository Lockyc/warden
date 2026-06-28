mod ffi;
mod geometry;
mod plan;
mod surface;

#[cfg(not(target_os = "macos"))]
compile_error!(
    "warden-app currently targets macOS only (libghostty surface embed). Linux is a later spike."
);

#[cfg(target_os = "macos")]
mod manager;

#[cfg(target_os = "macos")]
mod registry;

#[cfg(target_os = "macos")]
use manager::{InitDto, WindowManager};

use geometry::WebRect;

// Menu-item IDs, matched in the Builder's on_menu_event handler.
// Direct-jump items use the prefix `tab_jump_<n>` (1-based position).
const MENU_TAB_PREV: &str = "tab_prev";
const MENU_TAB_NEXT: &str = "tab_next";
const MENU_TAB_CLOSE: &str = "tab_close";
const MENU_TAB_JUMP_PREFIX: &str = "tab_jump_";
const MENU_WINDOW_CLOSE: &str = "window_close";

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

/// Kill tab `id`'s terminal (surface + PTY) in the calling window; it goes cold and
/// respawns fresh on next focus. Returns the id of the tab that became active if the
/// killed one was visible (so the chrome moves its highlight there), else `None`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn unload_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) -> Option<String> {
    let mut m = state.0.lock().unwrap();
    m.windows
        .get_mut(window.label())
        .and_then(|ws| ws.registry.unload(&id))
}

/// Update the calling window's active-surface frame from a web-coordinate rect.
#[cfg(target_os = "macos")]
#[tauri::command]
fn set_hole_rect(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, rect: RectArg) {
    // Reject non-finite values before they reach NSView or libghostty.
    if !rect.x.is_finite()
        || !rect.y.is_finite()
        || !rect.width.is_finite()
        || !rect.height.is_finite()
    {
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
    let view_rect = geometry::web_rect_to_view(
        WebRect {
            x,
            y,
            width,
            height,
        },
        view_h,
    );

    let mut m = state.0.lock().unwrap();
    if let Some(ws) = m.windows.get_mut(window.label()) {
        ws.registry.set_active_frame(view_rect);
    }
}

/// Message the diagnostic window displays — read by `diagnostic.html` on load.
#[cfg(target_os = "macos")]
#[tauri::command]
fn diagnostic_message(state: tauri::State<ManagerState>) -> String {
    state.0.lock().unwrap().diagnostic_msg.clone()
}

/// Remove tmux's `$TMUX`/`$TMUX_PANE` from warden-app's own environment so the shells it
/// spawns never inherit them. tmux exports these into every process under a pane, and
/// warden-app is routinely launched from inside a tmux session — e.g. the very agentmux
/// session warden exists to *host*. libghostty gives each surface's shell warden-app's
/// environment verbatim, so without this scrub every tab inherits a stale `$TMUX`; tmux-based
/// tools (`amux`) then believe they're nested and refuse to build their frame, and prefix keys
/// misroute. A terminal host must present a tmux-free base environment. Must run at the very
/// top of `main()`, before any thread starts or surface spawns.
fn scrub_inherited_tmux_env() {
    for var in ["TMUX", "TMUX_PANE"] {
        std::env::remove_var(var);
    }
}

fn main() {
    // warden hosts terminals — it must not leak its own launcher's tmux membership into them
    // (breaks nested agentmux/tmux). Scrub before anything else inherits the environment.
    scrub_inherited_tmux_env();

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
        // Menu items act on the focused window. Tab nav (⌘⇧[/⌘⇧], ⌘1–⌘9) and Close Tab (⌘W)
        // route through its chrome, which owns the tab list + select()/unload; emit_to targets
        // that one window so siblings don't also act. Close Window (⌘⇧W) closes it directly.
        // Unknown IDs (e.g. predefined Quit/Minimize, which self-handle) are ignored.
        .on_menu_event(|app, event| {
            use tauri::{Emitter, Manager};
            let Some(win) = app
                .webview_windows()
                .into_values()
                .find(|w| w.is_focused().unwrap_or(false))
            else {
                return;
            };
            let label = win.label().to_string();
            let id = event.id().as_ref();
            if id == MENU_TAB_PREV {
                let _ = app.emit_to(label.as_str(), "warden:cycle-tab", -1i32);
            } else if id == MENU_TAB_NEXT {
                let _ = app.emit_to(label.as_str(), "warden:cycle-tab", 1i32);
            } else if id == MENU_TAB_CLOSE {
                // ⌘W unloads the active tab (kill surface+PTY → cold, respawns on next focus),
                // it does NOT close the window. The chrome owns "which tab is active" + the
                // dot/highlight repaint, so it drives the unload_tab command on this event.
                let _ = app.emit_to(label.as_str(), "warden:unload-tab", ());
            } else if id == MENU_WINDOW_CLOSE {
                // ⌘⇧W closes the whole profile window (Destroyed → reap surfaces, last-window-quit).
                let _ = win.close();
            } else if let Some(n) = id
                .strip_prefix(MENU_TAB_JUMP_PREFIX)
                .and_then(|s| s.parse::<u32>().ok())
            {
                let _ = app.emit_to(label.as_str(), "warden:select-tab", n);
            }
        })
        .invoke_handler(tauri::generate_handler![
            set_hole_rect,
            init_tabs,
            activate_tab,
            unload_tab,
            diagnostic_message
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;

                let handle = app.handle().clone();
                let mut mgr = WindowManager::new();
                // Load config; on a missing/invalid/empty config, fall back to a
                // single diagnostic window instead of materializing profiles.
                // Recovery happens in the watcher: the first valid load while no
                // profile window is live materializes + closes the diagnostic.
                match warden_config::load(&warden_config::config_path()) {
                    Ok(loaded) if !loaded.config.profiles.is_empty() => {
                        mgr.materialize(&handle, loaded.config);
                    }
                    Ok(_) => mgr.show_diagnostic(&handle, "config has no [[profile]] entries"),
                    Err(e) => mgr.show_diagnostic(&handle, &e.to_string()),
                }
                app.manage(ManagerState(std::sync::Mutex::new(mgr)));

                // macOS menu. Windows are built at runtime with no NSMenu, so without this the
                // standard shortcuts are dead and there's nowhere to surface tab navigation.
                // Predefined items (Minimize/Quit) self-handle; custom items fire the Builder's
                // on_menu_event. Tab chords ⌘⇧[/⌘⇧] (prev/next) and ⌘1–⌘9 (jump) are macOS-
                // standard and checked app-wide before any view, so they never collide with the
                // terminal. ⌘W unloads the active *tab* and ⌘⇧W closes the *window* — the Safari/
                // Chrome convention (close-tab vs close-window), NOT the predefined ⌘W=close-window.
                {
                    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
                    let close_window = MenuItemBuilder::with_id(MENU_WINDOW_CLOSE, "Close Window")
                        .accelerator("Shift+Cmd+KeyW")
                        .build(app)?;
                    let app_menu = SubmenuBuilder::new(app, "warden")
                        .minimize()
                        .item(&close_window)
                        .separator()
                        .quit()
                        .build()?;

                    let prev = MenuItemBuilder::with_id(MENU_TAB_PREV, "Previous Tab")
                        .accelerator("Shift+Cmd+BracketLeft")
                        .build(app)?;
                    let next = MenuItemBuilder::with_id(MENU_TAB_NEXT, "Next Tab")
                        .accelerator("Shift+Cmd+BracketRight")
                        .build(app)?;
                    let close_tab = MenuItemBuilder::with_id(MENU_TAB_CLOSE, "Close Tab")
                        .accelerator("Cmd+KeyW")
                        .build(app)?;
                    let mut tab_menu = SubmenuBuilder::new(app, "Tab")
                        .item(&prev)
                        .item(&next)
                        .separator()
                        .item(&close_tab)
                        .separator();
                    // ⌘1–⌘9 jump straight to the tab at that position (no-op past the last tab).
                    let jumps = (1..=9)
                        .map(|i| {
                            MenuItemBuilder::with_id(
                                format!("{MENU_TAB_JUMP_PREFIX}{i}"),
                                format!("Tab {i}"),
                            )
                            .accelerator(format!("Cmd+Digit{i}"))
                            .build(app)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    for j in &jumps {
                        tab_menu = tab_menu.item(j);
                    }
                    let tab_menu = tab_menu.build()?;

                    let menu = MenuBuilder::new(app)
                        .item(&app_menu)
                        .item(&tab_menu)
                        .build()?;
                    app.set_menu(menu)?;
                }

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
                            Ok(loaded) if !loaded.config.profiles.is_empty() => {
                                let st = wh.state::<ManagerState>();
                                let mut m = st.0.lock().unwrap();
                                if m.is_empty() {
                                    // Recovery: nothing live (launched into the
                                    // diagnostic window). Materialize from scratch
                                    // and close the diagnostic, rather than
                                    // reconciling against an empty last_good.
                                    m.materialize(&wh, loaded.config.clone());
                                    m.clear_diagnostic(&wh);
                                } else {
                                    let recon =
                                        warden_config::reconcile(&m.last_good, &loaded.config);
                                    m.apply(&wh, &recon);
                                    // Advance the reconcile baseline ONLY on a valid load.
                                    m.last_good = loaded.config.clone();
                                }
                                // Clear any stale error banner.
                                let _ = wh.emit("warden:error-clear", ());
                            }
                            Ok(_) => {
                                // Valid TOML but no profiles: keep live windows up,
                                // surface the error banner rather than tearing down.
                                let _ = wh.emit(
                                    "warden:error",
                                    "config has no [[profile]] entries".to_string(),
                                );
                            }
                            Err(e) => {
                                // Keep last_good; surface the parse error in the banner.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_removes_inherited_tmux_vars() {
        // Simulate warden-app launched from inside a tmux pane (the agentmux session it hosts).
        std::env::set_var("TMUX", "/tmp/tmux-501/agentmux-term,2109,29");
        std::env::set_var("TMUX_PANE", "%43");
        scrub_inherited_tmux_env();
        assert!(std::env::var_os("TMUX").is_none(), "TMUX must be scrubbed");
        assert!(
            std::env::var_os("TMUX_PANE").is_none(),
            "TMUX_PANE must be scrubbed"
        );
    }
}
