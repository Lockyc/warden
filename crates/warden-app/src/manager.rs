//! Owns the live window windows. Materializes them from config and (Task 7)
//! applies reconciliations. Impure (Tauri + AppKit) — verified at checkpoints.

use crate::plan::{reconcile_ops, window_specs, WindowOp, WindowSpec};
use crate::probe::Presence;
use crate::registry::{ProbeTarget, Registry, TabDto};
use crate::surface::PixelRect;
use crate::ManagerState;
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// One window's probe work-list: `(window label, its probe-enabled tabs)`.
pub type WindowProbeTargets = (String, Vec<ProbeTarget>);

/// Last-known session presence per tab, keyed window-label → tab-id → `Presence` (on/ghost/off).
///
/// Owned by the `WindowManager` and **deliberately outliving the per-window Registry**:
/// the presence dot's only live source is an async `warden:session-state` event, which a
/// freshly-built webview isn't yet listening for, so on window open/reopen the first probe
/// emits are dropped and the dots stay hollow until the chrome finishes booting and
/// re-drives a probe. This cache breaks that: the scheduler records every probe result here
/// as it lands (`record_one`, before the emit that may be dropped), and `init_dto`/refresh
/// patch each `TabDto.presence` from it, so a (re)opened window
/// paints its dots on the FIRST render from the last-known state (self-correcting — the
/// live burst overwrites any staleness within a pass). Surviving close/reopen is the whole
/// point: a reopen rebuilds the Registry from scratch, so a Registry-local cache would be
/// empty exactly when it's needed.
#[derive(Default)]
pub struct PresenceCache {
    by_window: BTreeMap<String, BTreeMap<String, Presence>>,
}

impl PresenceCache {
    /// Record ONE tab's probe result (id → `Presence`) the moment its probe returns — never a
    /// whole pass at the end of the sweep. The granularity is load-bearing: a sweep that is
    /// still in flight when the chrome's listener registers must already have its finished
    /// tabs in the cache, because `probe_now`'s replay reads the cache at exactly that instant
    /// (see `probe::probe_window`, which records before it emits).
    pub fn record_one(&mut self, label: &str, id: &str, on: Presence) {
        self.by_window
            .entry(label.to_string())
            .or_default()
            .insert(id.to_string(), on);
    }

    /// The last-known presence for a window's tabs (id → `Presence`), for replaying to a chrome
    /// whose `session-state` listener has just come up. Empty if no pass has recorded this window
    /// yet. Distinct from `patch` (which seeds `init_dto` at build): a probe pass that finishes
    /// *between* build and the listener registering drops its live per-tab emits, and — `prev` now
    /// populated — no later pass re-emits, so the dots would be stuck at their pre-pass (hollow)
    /// state; `probe_now` replays this snapshot at the handshake to close that gap.
    pub fn snapshot(&self, label: &str) -> BTreeMap<String, Presence> {
        self.by_window
            .get(label)
            .map(|m| m.iter().map(|(id, on)| (id.clone(), *on)).collect())
            .unwrap_or_default()
    }

    /// Fill each tab's `presence` from the cache; a tab the cache has never seen is left
    /// as-is (`None` from `tab_dtos`). What that renders as depends on `has_probe`, not on
    /// this field alone — see `TabDto.presence`'s doc (registry.rs): a probe-enabled tab
    /// paints a hollow "unknown" ring until its first probe lands, while a tab with no probe
    /// configured has no dot at all.
    pub fn patch(&self, label: &str, tabs: &mut [TabDto]) {
        let Some(m) = self.by_window.get(label) else {
            return;
        };
        for t in tabs.iter_mut() {
            if let Some(&on) = m.get(&t.id) {
                t.presence = Some(on);
            }
        }
    }
}

/// Expand a config's project-tree roots into the effective tab set: for each window,
/// append the scanner-synthesized project tabs for every `[[window.root]]` to `tabs`.
/// This is what the whole pipeline (window_specs / reconcile / registry) consumes, so a
/// discovered project is just a `Tab` and needs no special-casing downstream. Roots'
/// tabs come after loose + grouped tabs, matching the "sections in file order" rule.
pub fn effective_config(config: &Config) -> Config {
    let mut eff = config.clone();
    for window in &mut eff.windows {
        for root in &window.roots {
            window.tabs.extend(crate::scanner::synthesize_tabs(root));
        }
        // Dedup by key (curated tabs first, then roots in file order), keeping the FIRST
        // occurrence. Two cases collapse here:
        //  - Overlapping roots synthesizing the same project (same path key) → land once.
        //  - A curated tab and a `[[window.root]]`-discovered project at the SAME dir now
        //    share a key (curated key = normalized dir; discovered key = path), so the
        //    curated tab shadows the discovered one. Intended: no "same repo twice" and no
        //    curated-vs-discovered key collision (reconcile matches find-first).
        let mut seen = HashSet::new();
        window.tabs.retain(|t| seen.insert(t.key.clone()));
    }
    eff
}

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
    /// Whether the sidebar chrome is a window-move drag handle, from the global
    /// `sidebar_drag` config (default true). Carried per-window (it's global) so
    /// every snapshot — init and hot-reload refresh — applies the current mode.
    pub sidebar_drag: bool,
    /// Whether warden checks for a new release on launch, from the global
    /// `auto_update` config (default true). Carried per-window (it's global); the
    /// chrome gates its launch-time update check on it (the menu check ignores it).
    pub auto_update: bool,
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
    /// The RAW config as loaded (roots UNexpanded). `last_good` is its expanded
    /// form (`effective_config(&raw_config)`, i.e. with discovered project tabs
    /// appended); `raw_config` is retained so `rescan_root` can re-expand + re-scan
    /// without a config-file change.
    pub raw_config: Config,
    /// Whether the most recent config load attempt succeeded. `None` = the last load was
    /// clean; `Some(msg)` carries the error text the home surface's `Broken` state shows,
    /// set on every failed load (missing file / parse / resolve error) and cleared on every
    /// successful one (`materialize_effective`, plus the watcher's own reconcile path — see
    /// `main.rs`). `sync_empty_surface` is called from places that carry no load context of
    /// their own (e.g. a window's `Destroyed` handler), so it can't take this as a parameter
    /// the way curator's `reconcile_home` does — it must persist here instead.
    pub load_error: Option<String>,
    /// The presence scheduler's slow-floor cadence in seconds; shared as an `Arc` with
    /// `probe::run_scheduler` so a hot-reload changes it live. 0 = event-driven only
    /// (burst on triggers, then Idle — no steady polling between events).
    pub probe_interval: Arc<AtomicU64>,
    /// Last-known session presence per tab, surviving window close/reopen so a (re)opened
    /// window's `init_dto`/refresh paints its cyan dots from the initial snapshot instead of
    /// waiting for the first post-boot probe emit (see `PresenceCache`). Written by the
    /// scheduler each probe pass (`probe::probe_window`), read when building DTOs.
    pub presence_cache: PresenceCache,
    /// Tauri labels of windows the user has closed, most-recent last (MRU stack).
    /// Drives `⌘⇧T` (Reopen Last Closed). Deduped on push so a repeated close
    /// moves the label to the end rather than growing duplicates. Pruned at reopen
    /// (`reopen_window` / `reopen_last`). Stale entries (closed-then-deleted, or
    /// already reopened) are filtered at reopen time.
    pub last_closed: Vec<String>,
}

impl WindowManager {
    pub fn new() -> Self {
        // `last_good` (effective) and `raw_config` (raw) start identical and empty;
        // clone rather than duplicate the literal so a new Config field can't drift
        // between them.
        let empty = Config {
            windows: Vec::new(),
            format_on_save: false,
            tab_digit_keys: warden_config::TabDigitKeys::default(),
            probe_interval: 5,
            density: warden_config::Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
        };
        WindowManager {
            windows: HashMap::new(),
            names: HashMap::new(),
            raw_config: empty.clone(),
            last_good: empty,
            load_error: None,
            probe_interval: Arc::new(AtomicU64::new(5)),
            presence_cache: PresenceCache::default(),
            last_closed: Vec::new(),
        }
    }

    /// The single authority for what shows when there may be zero real windows. Delegates
    /// entirely to `shell_core::home::home_state`, mirroring curator's `reconcile_home`:
    /// `home_state` itself already picks between real-windows-win (`None` → close), no config
    /// on disk (`NoConfig`), the last load's error (`Broken`), and the configured-window list
    /// (`Windows`, possibly empty — a config that parses but declares no `[[window]]` at all is
    /// simply an empty list, not an error). warden has no per-call load context the way
    /// curator's call sites do (this runs from places like a window's `Destroyed` handler, which
    /// carries none), so `self.load_error` is the persisted stand-in for curator's `load_error`
    /// parameter — every load call site sets or clears it (see `materialize_effective` and
    /// `main.rs`'s watcher). Called after launch materialize, the `Destroyed` handler, every
    /// hot-reload (success or failure), and every home-surface button click.
    pub fn sync_empty_surface(&mut self, app: &AppHandle) {
        let entries: Vec<shell_core::menu::WindowEntry> = self
            .window_menu_entries()
            .into_iter()
            .map(|e| shell_core::menu::WindowEntry {
                id: e.label,
                title: e.title,
                open: e.open,
                colour: Some(e.colour),
            })
            .collect();
        let path = warden_config::config_path();
        let path_str = path.display().to_string();
        match shell_core::home::home_state(
            !self.is_empty(),
            path.exists(),
            &path_str,
            self.load_error.as_deref(),
            &entries,
        ) {
            None => shell_core::home::close_home(app),
            Some(s) => {
                // shell_core::home::show_home is idempotent — it refreshes an already-open home
                // window rather than rebuilding one — so only attach the quit handler below the
                // FIRST time this window is actually built, mirroring the old show_launcher's
                // `if let Ok(w) = built` guard (it never re-attached on refresh either).
                let was_open = app
                    .get_webview_window(shell_core::home::HOME_LABEL)
                    .is_some();
                let _ = shell_core::home::show_home(app, &s, "warden");
                // KEPT WARDEN-LOCAL, not dropped: shell-core's `show_home` installs no
                // window-event handler of its own (curator/lector need none — plain
                // last-window-quit already covers them), but warden's former launcher always quit
                // the app when IT was closed while no real window existed ("closing the home
                // surface when it's the last surface == ⌘Q") — a deliberate, shipped behaviour,
                // not an incidental one. Re-installing it here keeps that intact rather than
                // silently changing what closing the last surface does in a notarized,
                // already-shipped app. Attached for every state (`NoConfig`/`Broken`/`Windows`)
                // now that they share one window, not just the old launcher's window-list case —
                // closing the home surface while it's the only surface should quit regardless of
                // which state it happens to be showing.
                if !was_open {
                    if let Some(w) = app.get_webview_window(shell_core::home::HOME_LABEL) {
                        let app_for_event = app.clone();
                        w.on_window_event(move |event| {
                            if let tauri::WindowEvent::Destroyed = event {
                                if let Some(st) = app_for_event.try_state::<ManagerState>() {
                                    let empty = st.lock().is_empty();
                                    if empty {
                                        app_for_event.exit(0);
                                    }
                                }
                            }
                        });
                    }
                }
            }
        }
    }

    /// Update the shared probe-pass cadence (the scheduler reads it each tick).
    pub fn set_probe_interval(&self, secs: u64) {
        self.probe_interval.store(secs, Ordering::Relaxed);
    }

    /// Probe work-lists grouped by window label. `only = Some(label)` restricts to
    /// one window; `None` = every window (the scheduler's per-tick reconcile / `bump_all`).
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
        // surfaces; `sync_empty_surface` shows the home surface once the last real
        // window goes away — this handler no longer quits. Idempotent
        // with `apply`'s `WindowOp::Close` (which removes the state before closing
        // the window): `HashMap::remove` returns `None` the second time and
        // `close_all` drains, so there is no double-free.
        let app_for_event = app.clone();
        let label_for_event = spec.label.clone();
        window.on_window_event(move |event| {
            match event {
                tauri::WindowEvent::Destroyed => {
                    if let Some(st) = app_for_event.try_state::<ManagerState>() {
                        {
                            let mut m = st.lock();
                            // Record this close so `⌘⇧T` can reopen it. Fires for
                            // manual close AND hot-reload removal; a no-longer-
                            // configured label is filtered at reopen time.
                            m.last_closed.retain(|l| l != &label_for_event);
                            m.last_closed.push(label_for_event.clone());
                            m.remove_window(&label_for_event);
                            // Persistent home: last-window-close no longer quits —
                            // it shows the home surface, in whichever state (window
                            // list, broken config, or no config) currently applies.
                            // ⌘Q is the only quit. This also fires for every window
                            // torn down during ⌘Q, including
                            // the last one — verified on-device that native
                            // `terminate:` wins that race, so the home surface is
                            // never presented; a `RunEvent`/`is_quitting` guard is
                            // not needed unless that stops holding.
                            m.sync_empty_surface(&app_for_event);
                        }
                        // Refresh the Window menu's checkmarks/(closed) tags. Lock
                        // dropped above; rebuild_menu re-locks (non-reentrant).
                        let _ = crate::rebuild_menu(&app_for_event);
                    }
                }
                // Focus → fast-burst this window's session dots so a just-focused window is current
                // within a burst pass (covers `probe_interval = 0`, which never steady-polls).
                tauri::WindowEvent::Focused(true) => {
                    crate::probe::bump(&label_for_event);
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

    /// Materialize every window as a window. Expands `config`'s project-tree
    /// roots via `effective_config` before building; `raw_config` keeps the
    /// unexpanded config for later re-scans (`rescan_root`), while `last_good`
    /// (the reconcile baseline) holds the expanded form so a reopened window
    /// still carries its discovered project tabs.
    pub fn materialize(&mut self, app: &AppHandle, config: Config) {
        let effective = effective_config(&config);
        self.materialize_effective(app, config, effective);
    }

    /// Materialize from an **already-scanned** effective config. Split out so a caller
    /// holding the `ManagerState` lock can run the recursive root scan (`effective_config`)
    /// *before* locking and pass the result in — the scan must never run under the lock
    /// (it would block the probe thread; mirrors probe.rs's snapshot-then-release rule).
    pub fn materialize_effective(&mut self, app: &AppHandle, config: Config, effective: Config) {
        // Every call here follows a successful load (setup, `shell_home_create_config`, the
        // watcher's recovery branch) — clear a stale error so `sync_empty_surface` below reads
        // the current status, not a previous failure's leftover `Broken` text. The watcher's
        // OTHER success path (reconcile against an existing baseline, main.rs) doesn't route
        // through here, so it clears `load_error` itself at the same point.
        self.load_error = None;
        self.set_probe_interval(config.probe_interval);
        for spec in window_specs(&effective)
            .into_iter()
            .filter(|s| s.open_on_start)
        {
            let state = self.build_window(app, &spec);
            self.names.insert(spec.title.clone(), spec.label.clone());
            self.windows.insert(spec.label.clone(), state);
        }
        self.raw_config = config;
        self.last_good = effective;
        self.sync_empty_surface(app);
    }

    pub fn init_dto(&self, label: &str) -> Option<InitDto> {
        self.windows.get(label).map(|ws| {
            // Patch presence from the persistent cache so a (re)opened window paints its
            // cyan dots on the first render, not after the chrome boots + re-probes.
            let mut tabs = ws.registry.tab_dtos();
            self.presence_cache.patch(label, &mut tabs);
            InitDto {
                label: label.to_string(),
                title: ws.title.clone(),
                colour: ws.colour.clone(),
                density: self.last_good.density.as_str().to_string(),
                sidebar_drag: self.last_good.sidebar_drag,
                auto_update: self.last_good.auto_update,
                tabs,
                error: ws.spawn_error.clone(),
            }
        })
    }

    /// Re-emit every open window's snapshot as `warden:refresh` so each chrome rebuilds with the
    /// current global state. Used for a global-setting change (e.g. `density`) that produces no
    /// per-window reconcile op — `apply()` only emits for windows with a diff, so a density-only
    /// edit would otherwise never reach the chrome. Reuses `init_dto`, which carries the live
    /// density from `last_good` (already advanced to the new config by the caller).
    pub fn refresh_all_chrome(&self, app: &AppHandle) {
        for label in self.windows.keys() {
            if let Some(dto) = self.init_dto(label) {
                let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
            }
        }
    }

    /// Unload the tab owning `surface_id` back to **cold** because its child process exited, and
    /// tell that window's chrome. Returns nothing; a surface that no longer maps to a tab (already
    /// unloaded) is simply dropped.
    ///
    /// Without this, a dead tab is a dead end: libghostty keeps the surface alive rendering its
    /// "Process exited" overlay, warden's dot still reads *live*, and nothing short of ⌘W clears it.
    /// warden's tabs are config-declared and respawnable, so the honest state for "its process is
    /// gone" is exactly the state a manual unload produces — cold dot, surface freed, focus the row
    /// to spawn a fresh one. So this reuses `Registry::unload` rather than inventing a second
    /// teardown: same neighbour-leaning when the dead tab was the visible one, same chrome update
    /// (`warden:tab-exited` carries the id + the new active tab, and the chrome runs the identical
    /// tail it runs for the dot-✕ / ⌘W path). One dead-tab semantic, two triggers.
    pub fn handle_child_exited(app: &AppHandle, surface_id: usize) {
        let state = app.state::<ManagerState>();
        let Some((label, tab, _visible)) = state.lock().locate_surface(surface_id) else {
            return;
        };
        let new_active = state
            .lock()
            .windows
            .get_mut(&label)
            .and_then(|ws| ws.registry.unload(&tab));
        // Per-window event: `emit_to` leaks to sibling webviews, so stamp the label and let the
        // chrome filter (see CLAUDE.md).
        let _ = app.emit_to(
            label.as_str(),
            "warden:tab-exited",
            serde_json::json!({ "label": label, "id": tab, "newActive": new_active }),
        );
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

    /// Specs for every configured window (config order), with labels **consistent
    /// with the live window set** — the single source of truth for the Window menu
    /// and every reopen path.
    ///
    /// An open window keeps its **actual live label** (from `self.names`); a live
    /// Tauri window can't be relabeled, so the menu/reopen mapping has to match it.
    /// Closed-but-configured windows get a deterministic fresh label that avoids
    /// every already-assigned label. Recomputing labels purely from config order
    /// (`window_specs`) instead diverges from a live window's label whenever two
    /// titles sanitize to the same base and were introduced in an order that made
    /// `reconcile_ops` (which seeds from live labels) suffix a different one — which
    /// made the menu raise the wrong window, reopen build a duplicate, and `⌘⇧T`
    /// miss the closed window. Pinning open windows here removes that divergence.
    /// Pure logic lives in `plan::configured_specs` (unit-tested); this just supplies
    /// the live state (title→label map + the in-use label set).
    fn configured_specs(&self) -> Vec<WindowSpec> {
        crate::plan::configured_specs(&self.last_good, &self.names, &self.taken_labels())
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Drop a window's state and reap its surfaces, without re-closing the Tauri
    /// window (used from the `Destroyed` handler, where the OS already destroyed
    /// the window — calling `window.close()` again would be redundant). Surfaces
    /// are freed eagerly via `close_all` rather than relying on `GhosttySurface`'s
    /// `Drop` safety net, so the libghostty handles go with the window, not later.
    pub fn remove_window(&mut self, label: &str) {
        if let Some(mut ws) = self.windows.remove(label) {
            ws.registry.close_all();
            self.names.retain(|_, l| l != label);
        }
    }

    /// Menu rows for every configured window, tagged open/closed. Derived live
    /// from `last_good` + the live window set via `configured_specs` (labels pinned
    /// to live windows) — nothing persisted.
    pub fn window_menu_entries(&self) -> Vec<crate::plan::WindowMenuEntry> {
        let specs = self.configured_specs();
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
        let Some(spec) = self
            .configured_specs()
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
        let configured: HashSet<String> = self
            .configured_specs()
            .into_iter()
            .map(|s| s.label)
            .collect();
        let open: HashSet<String> = self.windows.keys().cloned().collect();
        match crate::plan::next_reopen_target(&self.last_closed, &configured, &open) {
            Some(label) => self.reopen_window(app, &label),
            None => false,
        }
    }

    /// Whether `⌘⇧T` / "Reopen Last Closed" has a reopenable target right now.
    pub fn has_reopen_target(&self) -> bool {
        let configured: HashSet<String> = self
            .configured_specs()
            .into_iter()
            .map(|s| s.label)
            .collect();
        let open: HashSet<String> = self.windows.keys().cloned().collect();
        crate::plan::next_reopen_target(&self.last_closed, &configured, &open).is_some()
    }

    /// Bring the live window set in line with a reloaded config by executing the
    /// `WindowOp`s the reconciliation produces. Open builds a window; Close tears
    /// down its surfaces and closes the Tauri window; Update mutates the registry
    /// in place and pushes a fresh snapshot so the chrome rebuilds its sidebar.
    /// `new_config` is the *new* effective config (roots already expanded) — its
    /// windows/roots are looked up by `reconcile_ops` to derive tree metadata for
    /// tabs added by this reconcile, and its global settings (density, sidebar_drag)
    /// are stamped into the refresh DTOs so a hot-reload that flips either updates the
    /// chrome (at apply time `self.last_good` is still the old config — the caller
    /// swaps it after apply).
    pub fn apply(&mut self, app: &AppHandle, recon: &Reconciliation, new_config: &Config) {
        let ops = reconcile_ops(recon, new_config, &self.names, &self.taken_labels());
        let density = new_config.density.as_str();
        let sidebar_drag = new_config.sidebar_drag;
        let auto_update = new_config.auto_update;
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
                    respawn_tabs,
                } => {
                    if let Some(ws) = self.windows.get_mut(&label) {
                        // Captured before any mutation below, so a respawn that hits the
                        // currently-visible tab can be re-activated once its surface is
                        // rebuilt (see the reactivate step after `reorder`).
                        let was_active = ws.registry.active_tab().map(str::to_string);
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
                            && respawn_tabs.is_empty()
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
                        // Respawn kept tabs whose terminal spec changed: tear down and
                        // rebuild by the same id (identity is stable). A cold tab just gets
                        // a fresh spec and lazy-spawns on next focus; a load_on_open tab
                        // respawns eagerly. The previously-active tab is re-activated below
                        // so a visible respawn shows its new surface immediately.
                        for tp in &respawn_tabs {
                            ws.registry.remove(&tp.spec.id);
                            if let Err(e) = ws.registry.add(&tp.spec, tp.load_on_open) {
                                eprintln!(
                                    "warden: surface respawn failed for tab {:?}: {e}",
                                    tp.spec.title
                                );
                            }
                        }
                        // Apply in-place metadata (group/probe/kill + recomputed
                        // tree/tree_path) without respawning; the warden:refresh below
                        // pushes fresh DTOs (has_probe/has_kill recomputed) and the
                        // post-reload bump_all fast-bursts every window. Tree-ness CAN
                        // flip for a *kept* tab: a curated tab whose normalized dir key
                        // equals a `[[window.root]]` discovery's path key shadows it, so
                        // reconcile sees the same key move between the root section
                        // (tree) and the curated group (not) — a set_meta, not add/
                        // remove. `reconcile_ops` recomputes `tree`/`tree_path` from the
                        // new group; applying them here keeps the shadowed row from
                        // rendering as a stale tree row (or vice-versa).
                        for m in &set_meta {
                            ws.registry
                                .set_meta(&m.id, &m.meta, m.tree, m.tree_path.clone());
                        }
                        ws.registry.reorder(&order);
                        // If the on-screen tab was respawned, it went cold — re-activate it
                        // so its new surface spawns and shows in place (no blank placeholder).
                        // Runs BEFORE tab_dtos below so the re-activated surface's spawned
                        // state is reflected in the refresh snapshot.
                        if let Some(active) = &was_active {
                            if respawn_tabs.iter().any(|tp| &tp.spec.id == active) {
                                if let Err(e) = ws.registry.activate(active) {
                                    eprintln!("warden: reactivate after respawn failed: {e}");
                                }
                            }
                        }
                        // Patch presence from the persistent cache (disjoint field from
                        // `self.windows` borrowed via `ws`, so this borrows cleanly) so a
                        // hot-reload refresh keeps the dots lit instead of blanking them
                        // until the next probe pass re-emits.
                        let mut tabs = ws.registry.tab_dtos();
                        self.presence_cache.patch(&label, &mut tabs);
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
                            sidebar_drag,
                            auto_update,
                            tabs,
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

    fn dto(id: &str) -> TabDto {
        TabDto {
            id: id.to_string(),
            title: id.to_string(),
            warn: false,
            spawned: false,
            group: None,
            has_probe: true,
            has_kill: false,
            has_cmd: false,
            tree: false,
            tree_path: Vec::new(),
            detached: false,
            presence: None,
        }
    }

    #[test]
    fn presence_cache_patches_recorded_tabs_and_survives_relookup() {
        let mut cache = PresenceCache::default();
        // A window's probe pass, recorded tab by tab as each probe returns: t0 present, t1 absent.
        cache.record_one("work", "t0", Presence::Present);
        cache.record_one("work", "t1", Presence::Absent);

        // A freshly-built window's DTOs (all presence: None) get patched from the cache —
        // this is what lets a reopened window paint its dots on first render.
        let mut tabs = vec![dto("t0"), dto("t1"), dto("t2")];
        cache.patch("work", &mut tabs);
        assert_eq!(
            tabs[0].presence,
            Some(Presence::Present),
            "t0 recorded present"
        );
        assert_eq!(
            tabs[1].presence,
            Some(Presence::Absent),
            "t1 recorded absent"
        );
        assert_eq!(tabs[2].presence, None, "t2 never probed → left unknown");

        // A window the cache has never seen leaves everything untouched.
        let mut other = vec![dto("t0")];
        cache.patch("personal", &mut other);
        assert_eq!(other[0].presence, None, "different window → no cross-talk");
    }

    #[test]
    fn presence_cache_snapshot_returns_recorded_state_for_handshake_replay() {
        let mut cache = PresenceCache::default();
        assert!(
            cache.snapshot("work").is_empty(),
            "no probe yet → empty snapshot (probe_now falls back to the bump)"
        );
        cache.record_one("work", "t0", Presence::Present);
        cache.record_one("work", "t1", Presence::Absent);
        // The snapshot is exactly what probe_now replays to a just-ready listener — including the
        // `Present` a lost emit left stuck (t0), which is the whole point of the replay.
        let expected: BTreeMap<String, Presence> = [
            ("t0".to_string(), Presence::Present),
            ("t1".to_string(), Presence::Absent),
        ]
        .into();
        assert_eq!(cache.snapshot("work"), expected);
        assert!(
            cache.snapshot("other").is_empty(),
            "different window → no cross-talk"
        );
    }

    #[test]
    fn presence_cache_snapshot_survives_a_recoverable_result() {
        // The exact bug this task closes: `probe_now`'s replay reads `snapshot`, and a
        // Recoverable tab caught between window-build and the chrome's listener registering
        // must come back as Recoverable, not collapse to Absent (the deleted `on ==
        // Presence::Present` bridge) or Present (the deleted `From<bool>` bridge).
        let mut cache = PresenceCache::default();
        cache.record_one("work", "ghost-tab", Presence::Recoverable);
        assert_eq!(
            cache.snapshot("work").get("ghost-tab"),
            Some(&Presence::Recoverable),
            "a recoverable session must replay as recoverable, not collapse to absent or present"
        );
    }

    #[test]
    fn presence_cache_snapshot_sees_a_tab_mid_sweep_not_only_at_pass_end() {
        // The stuck-dark-dot regression: probe_now's replay can land while a wide window's sweep is
        // still running, so a tab already probed must be visible in the snapshot immediately — not
        // only once the whole pass finishes.
        let mut cache = PresenceCache::default();
        cache.record_one("work", "t0", Presence::Present); // first tab of the sweep lands...
        assert_eq!(
            cache.snapshot("work").get("t0"),
            Some(&Presence::Present),
            "an in-flight sweep's finished tab must already be replayable"
        );
    }

    #[test]
    fn presence_cache_record_one_overwrites_with_the_newest_result() {
        let mut cache = PresenceCache::default();
        let mut tabs = vec![dto("t0")];
        cache.patch("work", &mut tabs);
        assert_eq!(tabs[0].presence, None, "never probed → no entry");

        cache.record_one("work", "t0", Presence::Present);
        cache.record_one("work", "t0", Presence::Absent); // a later pass (present→absent, e.g. a kill)
        let mut tabs = vec![dto("t0")];
        cache.patch("work", &mut tabs);
        assert_eq!(
            tabs[0].presence,
            Some(Presence::Absent),
            "newest result wins"
        );
    }

    #[test]
    fn presence_cache_round_trips_all_three_states() {
        let mut cache = PresenceCache::default();
        cache.record_one("w1", "t0", Presence::Present);
        cache.record_one("w1", "t1", Presence::Recoverable);
        cache.record_one("w1", "t2", Presence::Absent);

        let mut tabs = vec![dto("t0"), dto("t1"), dto("t2"), dto("t3")];
        cache.patch("w1", &mut tabs);

        assert_eq!(tabs[0].presence, Some(Presence::Present));
        assert_eq!(
            tabs[1].presence,
            Some(Presence::Recoverable),
            "ghost survives the cache"
        );
        assert_eq!(tabs[2].presence, Some(Presence::Absent));
        assert_eq!(
            tabs[3].presence, None,
            "never probed ⇒ unknown, distinct from Absent"
        );
    }

    #[test]
    fn presence_cache_record_one_overwrites_ghost_with_the_newest_result() {
        // The restore landed: ghost → present. A stale ghost in the cache would repaint on reopen.
        let mut cache = PresenceCache::default();
        cache.record_one("w1", "t0", Presence::Recoverable);
        cache.record_one("w1", "t0", Presence::Present);

        let mut tabs = vec![dto("t0")];
        cache.patch("w1", &mut tabs);
        assert_eq!(tabs[0].presence, Some(Presence::Present));
    }

    #[test]
    fn tab_dto_presence_serializes_to_chrome_core_wire_values() {
        let mut t = dto("t0");
        t.presence = Some(Presence::Recoverable);
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["presence"], serde_json::json!("ghost"));

        t.presence = Some(Presence::Present);
        assert_eq!(
            serde_json::to_value(&t).unwrap()["presence"],
            serde_json::json!("on")
        );

        t.presence = None;
        assert_eq!(
            serde_json::to_value(&t).unwrap()["presence"],
            serde_json::Value::Null,
            "no cache record for this tab stays null on the wire (see TabDto.presence's doc for how the chrome renders it)"
        );
    }
}
