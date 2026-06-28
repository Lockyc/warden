//! Owns the live profile windows. Materializes them from config and (Task 7)
//! applies reconciliations. Impure (Tauri + AppKit) — verified at checkpoints.

use crate::plan::{reconcile_ops, window_specs, WindowOp, WindowSpec};
use crate::registry::{Registry, TabDto};
use crate::surface::PixelRect;
use crate::ManagerState;
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use warden_config::{Config, Reconciliation};

/// Initial surface rect: offset by the 160px sidebar so the surface never
/// overlaps it before the first JS rect report arrives. (Matches the spike.)
const INITIAL_RECT: PixelRect = PixelRect {
    x: 160.0,
    y: 0.0,
    width: 740.0,
    height: 600.0,
};

/// The single diagnostic window's Tauri label. Deliberately NOT a profile
/// label and never inserted into `WindowManager::windows`, so it is invisible
/// to `is_empty()` and carries no `Destroyed`→last-window-quit handler: closing
/// it alone never exits the app, and it never counts as a "live" profile set.
const DIAG_LABEL: &str = "warden-diagnostic";

#[derive(serde::Serialize, Clone)]
pub struct InitDto {
    /// The Tauri window label this snapshot describes. The chrome records it on
    /// init and uses it to ignore `warden:refresh` events addressed to a sibling
    /// window — a robust per-window guard independent of emit_to's targeting.
    pub label: String,
    pub name: String,
    pub colour: String,
    pub tabs: Vec<TabDto>,
}

pub struct WindowState {
    pub window: WebviewWindow,
    pub registry: Registry,
    pub name: String,
    pub colour: String,
}

pub struct WindowManager {
    pub windows: HashMap<String, WindowState>, // key = Tauri label
    pub names: HashMap<String, String>,        // profile name -> label
    pub last_good: Config,
    /// The message shown by the diagnostic window; fetched by its page via the
    /// `diagnostic_message` command. Empty when no diagnostic is showing.
    pub diagnostic_msg: String,
}

impl WindowManager {
    pub fn new() -> Self {
        WindowManager {
            windows: HashMap::new(),
            names: HashMap::new(),
            last_good: Config {
                profiles: Vec::new(),
            },
            diagnostic_msg: String::new(),
        }
    }

    /// Open (or update) the single diagnostic window with `message`. Used at
    /// launch when the config is missing/invalid/empty, and during hot-reload
    /// recovery this window is closed by `clear_diagnostic`. Idempotent: if the
    /// window already exists, only the message is refreshed (the page re-fetches
    /// it on its own load; an already-open window keeps its stale text, which is
    /// acceptable since the banner path covers live edits).
    pub fn show_diagnostic(&mut self, app: &AppHandle, message: &str) {
        self.diagnostic_msg = message.to_string();
        if app.get_webview_window(DIAG_LABEL).is_none() {
            let _ = WebviewWindowBuilder::new(
                app,
                DIAG_LABEL,
                WebviewUrl::App("diagnostic.html".into()),
            )
            .title("warden")
            .inner_size(560.0, 320.0)
            .build();
        }
    }

    /// Close the diagnostic window if it is open (on recovery to a valid config).
    pub fn clear_diagnostic(&mut self, app: &AppHandle) {
        self.diagnostic_msg = String::new();
        if let Some(w) = app.get_webview_window(DIAG_LABEL) {
            let _ = w.close();
        }
    }

    /// Build one Tauri window for `spec`, mount its tabs, activate the first.
    /// Returns the new `WindowState` (caller inserts it + wires events).
    pub fn build_window(&self, app: &AppHandle, spec: &WindowSpec) -> WindowState {
        let window =
            WebviewWindowBuilder::new(app, &spec.label, WebviewUrl::App("index.html".into()))
                .title(&spec.name)
                .inner_size(900.0, 600.0)
                .transparent(true)
                // Full-size content view (Overlay): the WKWebView + native surface
                // span the WHOLE window, including under the title bar, so the
                // terminal reaches the very top (curator-style). The title bar
                // becomes a transparent overlay; traffic lights stay visible over
                // the sidebar's top-left. `hidden_title` drops the title text so
                // only the in-app banner names the profile.
                .hidden_title(true)
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .build()
                .expect("build profile window");

        let ns_window = window.ns_window().expect("ns_window") as *mut std::os::raw::c_void;

        let mut registry = Registry::new(ns_window, INITIAL_RECT);
        for t in &spec.tabs {
            registry.add(&t.spec, t.keep_alive);
        }
        if let Some(first) = spec.tabs.first() {
            registry.activate(&first.spec.id);
        }

        // On manual close (or any destroy), drop the window's state and reap its
        // surfaces; quit when the last profile window goes away. Idempotent with
        // `apply`'s `WindowOp::Close` (which removes the state before closing the
        // window): `HashMap::remove` returns `None` the second time and
        // `close_all` drains, so there is no double-free.
        let app_for_event = app.clone();
        let label_for_event = spec.label.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(st) = app_for_event.try_state::<ManagerState>() {
                    let mut m = st.lock();
                    m.remove_window(&label_for_event);
                    if m.is_empty() {
                        app_for_event.exit(0);
                    }
                }
            }
        });

        WindowState {
            window,
            registry,
            name: spec.name.clone(),
            colour: spec.colour.clone(),
        }
    }

    /// Materialize every profile as a window. Sets `last_good = config`.
    pub fn materialize(&mut self, app: &AppHandle, config: Config) {
        for spec in window_specs(&config) {
            let state = self.build_window(app, &spec);
            self.names.insert(spec.name.clone(), spec.label.clone());
            self.windows.insert(spec.label.clone(), state);
        }
        self.last_good = config;
    }

    pub fn init_dto(&self, label: &str) -> Option<InitDto> {
        self.windows.get(label).map(|ws| InitDto {
            label: label.to_string(),
            name: ws.name.clone(),
            colour: ws.colour.clone(),
            tabs: ws.registry.tab_dtos(),
        })
    }

    /// Route a surface signal: find the (window-label, tab-id) owning surface `surface_id`, and
    /// whether that tab is currently **visible** (its window is focused AND it's the active tab).
    /// A visible tab needs no notification — the user is already looking at it.
    pub fn locate_surface(&self, surface_id: usize) -> Option<(String, String, bool)> {
        self.windows.iter().find_map(|(label, ws)| {
            let tab = ws.registry.tab_of_surface(surface_id)?;
            let focused = ws.window.is_focused().unwrap_or(false);
            let visible = focused && ws.registry.active_tab() == Some(tab);
            Some((label.clone(), tab.to_string(), visible))
        })
    }

    /// Labels currently in use — the seed `unique_label` must avoid when
    /// assigning a fresh label to a newly-opened profile during reconcile.
    fn taken_labels(&self) -> HashSet<String> {
        self.windows.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Drop a window's state and reap its surfaces, without re-closing the Tauri
    /// window (used from the `Destroyed` handler, where the OS already destroyed
    /// the window — calling `window.close()` again would be redundant). Surfaces
    /// must still be freed explicitly: `GhosttySurface` has no `Drop`, so merely
    /// dropping the registry would leak the libghostty surface handles.
    pub fn remove_window(&mut self, label: &str) {
        if let Some(mut ws) = self.windows.remove(label) {
            ws.registry.close_all();
            self.names.retain(|_, l| l != label);
        }
    }

    /// Bring the live window set in line with a reloaded config by executing the
    /// `WindowOp`s the reconciliation produces. Open builds a window; Close tears
    /// down its surfaces and closes the Tauri window; Update mutates the registry
    /// in place and pushes a fresh snapshot so the chrome rebuilds its sidebar.
    pub fn apply(&mut self, app: &AppHandle, recon: &Reconciliation) {
        let ops = reconcile_ops(recon, &self.names, &self.taken_labels());
        for op in ops {
            match op {
                WindowOp::Open(spec) => {
                    let state = self.build_window(app, &spec);
                    self.names.insert(spec.name.clone(), spec.label.clone());
                    self.windows.insert(spec.label.clone(), state);
                }
                WindowOp::Close(label) => {
                    if let Some(mut ws) = self.windows.remove(&label) {
                        ws.registry.close_all();
                        // Safe to hold the ManagerState mutex across close(): the
                        // per-window Destroyed handler re-locks this same
                        // non-reentrant Mutex, but tao delivers WindowEvent::Destroyed
                        // asynchronously on a later event-loop turn (not synchronously
                        // inside close()), so there is no same-thread re-entrant
                        // deadlock. The handler then no-ops (state already removed).
                        let _ = ws.window.close();
                        self.names.retain(|_, l| l != &label);
                    }
                }
                WindowOp::Update {
                    label,
                    colour,
                    add_tabs,
                    remove_tabs,
                    order,
                } => {
                    if let Some(ws) = self.windows.get_mut(&label) {
                        // An icon-only config change reaches here as an otherwise-empty
                        // Update: per-profile window icon isn't applied yet (deferred),
                        // and `order` still carries the unchanged tab sequence. Skip it
                        // so an icon edit doesn't churn a full sidebar rebuild for zero
                        // visible change.
                        let current_order: Vec<String> =
                            ws.registry.tab_dtos().into_iter().map(|t| t.id).collect();
                        if colour.is_none()
                            && add_tabs.is_empty()
                            && remove_tabs.is_empty()
                            && order == current_order
                        {
                            continue;
                        }
                        if let Some(c) = colour {
                            ws.colour = c;
                        }
                        for id in &remove_tabs {
                            ws.registry.remove(id);
                        }
                        for tp in &add_tabs {
                            ws.registry.add(&tp.spec, tp.keep_alive);
                        }
                        ws.registry.reorder(&order);
                        // Push the new snapshot so the chrome rebuilds the sidebar.
                        // Target THIS window by label: `Emitter::emit` (on a window
                        // OR the app handle) is a global broadcast in Tauri 2.11.3 —
                        // it delegates to the shared app manager regardless of the
                        // receiver — so emitting on `ws.window` would fire every
                        // sibling window's listener and corrupt their sidebars with
                        // this profile's DTO. `emit_to(label, …)` scopes it to the
                        // one window. `label` is the Tauri window label.
                        let dto = InitDto {
                            label: label.clone(),
                            name: ws.name.clone(),
                            colour: ws.colour.clone(),
                            tabs: ws.registry.tab_dtos(),
                        };
                        let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
                    }
                }
            }
        }
    }
}
