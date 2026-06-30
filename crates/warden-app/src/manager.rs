//! Owns the live window windows. Materializes them from config and (Task 7)
//! applies reconciliations. Impure (Tauri + AppKit) — verified at checkpoints.

use crate::plan::{reconcile_ops, window_specs, WindowOp, WindowSpec};
use crate::registry::{ProbeTarget, Registry, TabDto};
use crate::surface::PixelRect;
use crate::ManagerState;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

/// The single diagnostic window's Tauri label. Deliberately NOT a window
/// label and never inserted into `WindowManager::windows`, so it is invisible
/// to `is_empty()` and carries no `Destroyed`→last-window-quit handler: closing
/// it alone never exits the app, and it never counts as a "live" window set.
pub const DIAG_LABEL: &str = "warden-diagnostic";

/// One window's probe work-list: `(window label, its probe-enabled tabs)`.
pub type WindowProbeTargets = (String, Vec<ProbeTarget>);

#[derive(serde::Serialize, Clone)]
pub struct InitDto {
    /// The Tauri window label this snapshot describes. The chrome records it on
    /// init and uses it to ignore `warden:refresh` events addressed to a sibling
    /// window — a robust per-window guard independent of emit_to's targeting.
    pub label: String,
    pub title: String,
    pub colour: String,
    /// The whole-app chrome density token ("comfortable" | "compact"), from the
    /// global config. The chrome sets it as `data-density` on the root so its CSS
    /// variables switch sizing. Carried per-window (it's global) so every window's
    /// snapshot — init and hot-reload refresh — applies the current mode.
    pub density: String,
    pub tabs: Vec<TabDto>,
    /// A surface-spawn failure that happened while building this window, surfaced
    /// in the chrome's error banner on init. `None` = all tabs built cleanly. This
    /// is the launch channel for spawn errors: `build_window` runs before the
    /// webview registers its `warden:error` listener, so a pushed event would be
    /// lost — the chrome pulls this with the snapshot instead.
    pub error: Option<String>,
}

pub struct WindowState {
    pub window: WebviewWindow,
    pub registry: Registry,
    pub title: String,
    pub colour: String,
    /// Surface-spawn failure(s) from `build_window`, shown once via the init DTO.
    pub spawn_error: Option<String>,
}

pub struct WindowManager {
    pub windows: HashMap<String, WindowState>, // key = Tauri label
    pub names: HashMap<String, String>,        // window title -> label
    pub last_good: Config,
    /// The message shown by the diagnostic window; fetched by its page via the
    /// `diagnostic_message` command. Empty when no diagnostic is showing.
    pub diagnostic_msg: String,
    /// Seconds between background probe passes; shared with the poll thread so a
    /// hot-reload can change cadence live. 0 = focus/refresh-only (no timer).
    pub probe_interval: Arc<AtomicU64>,
    /// Tauri labels of windows the user has closed, most-recent last. Drives
    /// `⌘⇧T` (Reopen Last Closed). Filtered against the live configured/open sets
    /// at reopen time, so stale entries (closed-then-deleted, or already reopened)
    /// are skipped rather than pruned eagerly.
    pub last_closed: Vec<String>,
}

impl WindowManager {
    pub fn new() -> Self {
        WindowManager {
            windows: HashMap::new(),
            names: HashMap::new(),
            last_good: Config {
                windows: Vec::new(),
                format_on_save: false,
                tab_digit_keys: warden_config::TabDigitKeys::default(),
                probe_interval: 5,
                density: warden_config::Density::default(),
            },
            diagnostic_msg: String::new(),
            probe_interval: Arc::new(AtomicU64::new(5)),
            last_closed: Vec::new(),
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

    /// Update the shared probe-pass cadence (the poll thread reads it each tick).
    pub fn set_probe_interval(&self, secs: u64) {
        self.probe_interval.store(secs, Ordering::Relaxed);
    }

    /// Probe work-lists grouped by window label. `only = Some(label)` restricts to
    /// one window (focus trigger); `None` = every window (timer/refresh).
    pub fn probe_targets(&self, only: Option<&str>) -> Vec<WindowProbeTargets> {
        self.windows
            .iter()
            .filter(|(label, _)| only.is_none_or(|o| o == label.as_str()))
            .map(|(label, ws)| (label.clone(), ws.registry.probe_targets()))
            .collect()
    }

    /// Build one Tauri window for `spec`, mount its tabs, activate the first.
    /// Returns the new `WindowState` (caller inserts it + wires events).
    pub fn build_window(&self, app: &AppHandle, spec: &WindowSpec) -> WindowState {
        let window =
            WebviewWindowBuilder::new(app, &spec.label, WebviewUrl::App("index.html".into()))
                .title(&spec.title)
                .inner_size(spec.width, spec.height)
                .transparent(true)
                // Full-size content view (Overlay): the WKWebView + native surface
                // span the WHOLE window, including under the title bar, so the
                // terminal reaches the very top (curator-style). The title bar
                // becomes a transparent overlay; traffic lights stay visible over
                // the sidebar's top-left. `hidden_title` drops the title text so
                // only the in-app banner names the window.
                .hidden_title(true)
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .build()
                .expect("build window window");

        // Windows are built at runtime (not from tauri.conf.json), so the
        // window-state plugin's automatic restore doesn't apply — trigger it
        // explicitly. Saved bounds (keyed by the stable per-label) override the
        // config-resolved builder default above (spec.width × spec.height,
        // 1500×1000 by default); first launch (no saved state) keeps it.
        {
            use tauri_plugin_window_state::{StateFlags, WindowExt};
            let _ = window
                .restore_state(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED);
        }

        let ns_window = window.ns_window().expect("ns_window") as *mut std::os::raw::c_void;

        let mut registry = Registry::new(ns_window, INITIAL_RECT);
        // Surface-create failures (a null libghostty surface, an interior-NUL in a
        // config dir/shell) must NOT panic the whole app at launch — the failing
        // tab stays cold and its reason is collected for the init banner; every
        // other tab and window still comes up.
        let mut spawn_errors: Vec<String> = Vec::new();
        for t in &spec.tabs {
            if let Err(e) = registry.add(&t.spec, t.load_on_open) {
                spawn_errors.push(format!("{}: {e}", t.spec.title));
            }
        }
        if let Some(first) = spec.tabs.first() {
            if let Err(e) = registry.activate(&first.spec.id) {
                let msg = format!("{}: {e}", first.spec.title);
                // The first tab may have already failed its eager add above; don't
                // report the same tab twice.
                if !spawn_errors.contains(&msg) {
                    spawn_errors.push(msg);
                }
            }
        }
        let spawn_error = if spawn_errors.is_empty() {
            None
        } else {
            let joined = spawn_errors.join("; ");
            eprintln!(
                "warden: surface spawn failed in window {:?}: {joined}",
                spec.title
            );
            Some(format!("couldn't open terminal — {joined}"))
        };

        // On manual close (or any destroy), drop the window's state and reap its
        // surfaces; quit when the last window window goes away. Idempotent with
        // `apply`'s `WindowOp::Close` (which removes the state before closing the
        // window): `HashMap::remove` returns `None` the second time and
        // `close_all` drains, so there is no double-free.
        let app_for_event = app.clone();
        let label_for_event = spec.label.clone();
        window.on_window_event(move |event| {
            match event {
                tauri::WindowEvent::Destroyed => {
                    if let Some(st) = app_for_event.try_state::<ManagerState>() {
                        // Record this close so `⌘⇧T` can reopen it. Fires for manual
                        // close AND hot-reload removal; a no-longer-configured label is
                        // filtered out at reopen time, so pushing unconditionally is safe.
                        let exited = {
                            let mut m = st.lock();
                            m.last_closed.push(label_for_event.clone());
                            m.remove_window(&label_for_event);
                            if m.is_empty() {
                                app_for_event.exit(0);
                                true
                            } else {
                                false
                            }
                        };
                        // Refresh the Window menu's checkmarks/(closed) tags. Lock is
                        // dropped above; rebuild_menu re-locks (non-reentrant mutex).
                        if !exited {
                            let _ = crate::rebuild_menu(&app_for_event);
                        }
                    }
                }
                // Refresh this window's session dots when it gains focus — covers
                // `probe_interval = 0` (no timer) and keeps a just-focused window current.
                tauri::WindowEvent::Focused(true) => {
                    crate::probe::spawn_pass(app_for_event.clone(), Some(label_for_event.clone()));
                }
                _ => {}
            }
        });

        WindowState {
            window,
            registry,
            title: spec.title.clone(),
            colour: spec.colour.clone(),
            spawn_error,
        }
    }

    /// Materialize every window as a window. Sets `last_good = config`.
    pub fn materialize(&mut self, app: &AppHandle, config: Config) {
        self.set_probe_interval(config.probe_interval);
        for spec in window_specs(&config) {
            let state = self.build_window(app, &spec);
            self.names.insert(spec.title.clone(), spec.label.clone());
            self.windows.insert(spec.label.clone(), state);
        }
        self.last_good = config;
    }

    pub fn init_dto(&self, label: &str) -> Option<InitDto> {
        self.windows.get(label).map(|ws| InitDto {
            label: label.to_string(),
            title: ws.title.clone(),
            colour: ws.colour.clone(),
            density: self.last_good.density.as_str().to_string(),
            tabs: ws.registry.tab_dtos(),
            error: ws.spawn_error.clone(),
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
    /// assigning a fresh label to a newly-opened window during reconcile.
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

    /// Menu rows for every configured window, tagged open/closed. Derived live
    /// from `last_good` (deterministic labels via `window_specs`) and the live
    /// `windows` keyset — nothing persisted.
    pub fn window_menu_entries(&self) -> Vec<crate::plan::WindowMenuEntry> {
        let specs = window_specs(&self.last_good);
        let open: HashSet<String> = self.windows.keys().cloned().collect();
        crate::plan::window_menu_entries(&specs, &open)
    }

    /// Raise `label`'s window (unminimize + focus) if it is open. No-op otherwise.
    pub fn focus_window(&self, label: &str) {
        if let Some(ws) = self.windows.get(label) {
            let _ = ws.window.unminimize();
            let _ = ws.window.set_focus();
        }
    }

    /// Rebuild a closed window from its config spec (same label ⇒ saved bounds
    /// restore). Returns `false` if already open or no longer configured.
    pub fn reopen_window(&mut self, app: &AppHandle, label: &str) -> bool {
        if self.windows.contains_key(label) {
            return false;
        }
        let Some(spec) = window_specs(&self.last_good)
            .into_iter()
            .find(|s| s.label == label)
        else {
            return false;
        };
        let state = self.build_window(app, &spec);
        self.names.insert(spec.title.clone(), spec.label.clone());
        self.windows.insert(spec.label.clone(), state);
        self.last_closed.retain(|l| l != label);
        true
    }

    /// Reopen the most-recently-closed reopenable window (`⌘⇧T`). Returns whether
    /// a window was reopened.
    pub fn reopen_last(&mut self, app: &AppHandle) -> bool {
        let configured: HashSet<String> = window_specs(&self.last_good)
            .into_iter()
            .map(|s| s.label)
            .collect();
        let open: HashSet<String> = self.windows.keys().cloned().collect();
        match crate::plan::next_reopen_target(&self.last_closed, &configured, &open) {
            Some(label) => self.reopen_window(app, &label),
            None => false,
        }
    }

    /// Bring the live window set in line with a reloaded config by executing the
    /// `WindowOp`s the reconciliation produces. Open builds a window; Close tears
    /// down its surfaces and closes the Tauri window; Update mutates the registry
    /// in place and pushes a fresh snapshot so the chrome rebuilds its sidebar.
    /// `density` is the *new* config's density token, stamped into the refresh DTOs
    /// so a hot-reload that flips density updates the chrome (at apply time
    /// `self.last_good` is still the old config — the caller swaps it after apply).
    pub fn apply(&mut self, app: &AppHandle, recon: &Reconciliation, density: &str) {
        let ops = reconcile_ops(recon, &self.names, &self.taken_labels());
        for op in ops {
            match op {
                WindowOp::Open(spec) => {
                    let state = self.build_window(app, &spec);
                    self.names.insert(spec.title.clone(), spec.label.clone());
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
                    set_meta,
                } => {
                    if let Some(ws) = self.windows.get_mut(&label) {
                        // Skip no-op updates (e.g. a config save that changes nothing
                        // visible). `order` still carries the unchanged tab sequence; a
                        // metadata change always carries `set_meta`, so it is never
                        // mistaken for a no-op.
                        let current_order: Vec<String> =
                            ws.registry.tab_dtos().into_iter().map(|t| t.id).collect();
                        if colour.is_none()
                            && add_tabs.is_empty()
                            && remove_tabs.is_empty()
                            && order == current_order
                            && set_meta.is_empty()
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
                            // A failed eager spawn on hot-reload leaves the tab cold
                            // (it retries on focus, which surfaces the error via the
                            // banner then) — log it, never panic.
                            if let Err(e) = ws.registry.add(&tp.spec, tp.load_on_open) {
                                eprintln!(
                                    "warden: surface spawn failed for tab {:?}: {e}",
                                    tp.spec.title
                                );
                            }
                        }
                        // Apply in-place metadata (group/probe/kill) without respawning;
                        // the warden:refresh below pushes fresh DTOs (has_probe/has_kill
                        // recomputed) and the post-reload spawn_pass re-probes.
                        for (id, meta) in &set_meta {
                            ws.registry
                                .set_meta(id, meta.group.clone(), meta.probe.clone(), meta.kill.clone());
                        }
                        ws.registry.reorder(&order);
                        // Push the new snapshot so the chrome rebuilds the sidebar.
                        // Target THIS window by label: `Emitter::emit` (on a window
                        // OR the app handle) is a global broadcast in Tauri 2.11.3 —
                        // it delegates to the shared app manager regardless of the
                        // receiver — so emitting on `ws.window` would fire every
                        // sibling window's listener and corrupt their sidebars with
                        // this window's DTO. `emit_to(label, …)` scopes it to the
                        // one window. `label` is the Tauri window label.
                        let dto = InitDto {
                            label: label.clone(),
                            title: ws.title.clone(),
                            colour: ws.colour.clone(),
                            density: density.to_string(),
                            tabs: ws.registry.tab_dtos(),
                            // Refresh carries no spawn error; a hot-reload add
                            // failure is logged + retried-on-focus, not banner-pushed.
                            error: None,
                        };
                        let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn probe_interval_defaults_to_5_and_is_settable() {
        let m = WindowManager::new();
        assert_eq!(m.probe_interval.load(Ordering::Relaxed), 5);
        m.set_probe_interval(0);
        assert_eq!(m.probe_interval.load(Ordering::Relaxed), 0);
    }
}
