//! Owns the live window windows. Materializes them from config and (Task 7)
//! applies reconciliations. Impure (Tauri + AppKit) — verified at checkpoints.

use crate::plan::{reconcile_ops, window_specs, TabPlan, WindowOp, WindowSpec};
use crate::probe::Presence;
use crate::registry::{ProbeTarget, Registry, TabDto};
use crate::surface::ghostty::GhosttySurface;
use crate::surface::{PixelRect, TerminalSurface};
use crate::ManagerState;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use warden_config::{Config, Reconciliation};

/// Initial surface rect: offset by the 160px sidebar so the surface never
/// overlaps it before the first JS rect report arrives. (Matches the spike.)
///
/// `pub(crate)` for one more caller than `redock`: `pop_out_tab`'s reparent-rollback needs
/// the same "any plausible rect, corrected by the next `activate`" placeholder, and a second
/// literal there would be a copy of this fact that could drift from it.
pub(crate) const INITIAL_RECT: PixelRect = PixelRect {
    x: 160.0,
    y: 0.0,
    width: 740.0,
    height: 600.0,
};

/// Set once on `RunEvent::ExitRequested` (main.rs), which fires before every window's
/// `Destroyed` during ⌘Q. Checked at the top of `redock` so a detached window's teardown
/// doesn't reopen its (already-closed) origin window mid-quit. Never reset — warden doesn't
/// prevent exit, so once it's true the app is on its way out for good.
static IS_QUITTING: AtomicBool = AtomicBool::new(false);

/// Mark the app as quitting. Call once, from `RunEvent::ExitRequested`.
pub fn mark_quitting() {
    IS_QUITTING.store(true, Ordering::SeqCst);
}

/// Whether the app is quitting — see `IS_QUITTING`.
pub fn is_quitting() -> bool {
    IS_QUITTING.load(Ordering::SeqCst)
}

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

/// A tab that has been popped out into its own detached window. Holds the live surface(s)
/// (moved out of its origin `WindowState`'s `Registry`, which keeps a `Detached`
/// placeholder in each slot) plus the bookkeeping needed to return them when the detached
/// window closes: which origin window/tab they belong to.
///
/// Pop-out is **whole-tab**, so a split tab brings its second pane along: `secondary` is
/// `Some` exactly when `Registry::detach` handed back a live second surface. The detached
/// window then lays out two holes and `set_detached_frame` routes each hole's rect to its
/// own surface.
///
/// These live in `WindowManager::detached`, **separate from `windows`**, so hot-reload
/// `reconcile` (which only walks `windows`) never sees them and can't close or duplicate
/// them — the detached-label prefix exclusion is thus never even reached for these,
/// because they aren't in the reconciled set at all. `is_empty` counts them so the home
/// surface doesn't pop up while a detached window is the only thing on screen.
/// The chrome half of retiring a popped-out tab's second pane, run with the lock RELEASED after
/// [`WindowManager::close_detached_secondary`]: the detached page collapses to one hole (the
/// surface is already gone, so its immediate re-report for pane 0 lands on the primary, and a
/// stale pane-1 rect is dropped by `set_detached_frame`), and the origin's chrome gets the same
/// `warden:pane-closed` a docked exit sends, so its persisted ratio goes too.
pub fn announce_detached_secondary_closed(
    app: &AppHandle,
    dlabel: &str,
    origin_label: &str,
    tab_id: &str,
) {
    let _ = shell_core::detach::set_panes(app, dlabel, &[]);
    let _ = app.emit_to(
        origin_label,
        "warden:pane-closed",
        serde_json::json!({ "label": origin_label, "id": tab_id }),
    );
}

/// Tear down a popped-out tab's surfaces on purpose — the one place a live PTY is
/// intentionally ended by the pop-out/redock flow (and its failure arms). Goes through
/// `close()`, never a bare `drop`: `GhosttySurface::drop` treats a surface dropped without
/// `close()` as a registry bug and says so in debug builds, so a bare drop here reported a
/// false bug on every deliberate teardown. One helper for every site so the pair can't be
/// half-closed.
pub(crate) fn close_both(primary: GhosttySurface, secondary: Option<GhosttySurface>) {
    primary.close();
    if let Some(s) = secondary {
        s.close();
    }
}

pub struct DetachedSurface {
    pub surface: GhosttySurface,
    /// The tab's second pane, when it had a live one at pop-out time. `None` for an
    /// unsplit tab (and for a split tab whose secondary was still cold — nothing live to
    /// carry, so the window opens with one hole rather than one it can't fill).
    pub secondary: Option<GhosttySurface>,
    pub origin_label: String,
    pub tab_id: String,
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
    /// Tabs currently popped out into their own detached windows, keyed by the detached
    /// window's Tauri label (`shell_core::detach::detached_label`). Kept **separate from
    /// `windows`** so hot-reload reconcile never touches them (it walks `windows` only) and
    /// so `is_empty` can count them (keeping the home surface hidden while a detached window
    /// is the only surface on screen). Populated by `pop_out_tab`, drained by `redock` when
    /// the detached window closes.
    pub detached: HashMap<String, DetachedSurface>,
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
            detached: HashMap::new(),
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

        // Saved bounds are restored by shell-core's geometry plugin, on its `on_window_ready`
        // hook — which fires for runtime-built windows too, so nothing needs to trigger it here.
        //
        // FOOTGUN: do NOT restore geometry by hand in this function. It looks like it should be
        // needed — this window is built at runtime, not from `tauri.conf.json` — but the plugin's
        // hook already covers that case. `build_window` runs from two call sites: the setup hook
        // (main thread, before the event loop starts spinning) and hot-reload (the watcher
        // thread). Reading/setting geometry marshals to the main loop, and tauri-runtime-wry's
        // `send_user_message` dispatches by thread id, not by whether the loop has started
        // spinning — so the setup-hook call would resolve inline, no hang there. The
        // watcher-thread call is genuinely off the main thread, so that marshal blocks and can
        // deadlock against the plugin's own hook running on reload.

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
    ///
    /// Child-exit is per-pane: a SECONDARY's child exiting closes just that pane — the scratch
    /// shell is done, the tab and its agent are not. Unloading the whole tab here (the pre-split
    /// behaviour) would kill a live agent because a shell beside it exited. A PRIMARY exit still
    /// emits `warden:tab-exited`; that event means "the whole tab went cold" to the chrome
    /// (`applyUnloaded`), which is wrong for a pane that merely closed — a SECONDARY exit instead
    /// emits the narrower `warden:pane-closed`, so the chrome collapses the split without
    /// touching the tab's own dot/highlight.
    ///
    /// **Popped-out surfaces route too.** A surface not found in any `windows` registry is looked
    /// up in `detached` — the same two outcomes, reached from the detached window: a secondary
    /// exit closes that surface and asks the shared detach page to relayout to one hole
    /// (`shell_core::detach::set_panes`), telling the origin's chrome the same `warden:pane-closed`
    /// the docked path would (so its persisted ratio goes too); a primary exit ends the tab — both
    /// surfaces closed, the origin's `Detached` placeholders retired to `Cold`
    /// (`Registry::clear_detached`), the detached window closed, and the origin's chrome told
    /// `warden:tab-exited` exactly as if the exit had happened docked. Before this the popped-out
    /// case was unrouted, and the "Process exited" overlay sat in the detached window until the
    /// user closed it by hand.
    pub fn handle_child_exited(app: &AppHandle, surface_id: usize) {
        let state = app.state::<ManagerState>();
        let mut lock = state.lock();
        // The WINDOW lookup only — its tab id is deliberately discarded. Resolving the tab twice
        // (once here, once from the window's own registry below) meant emitting `warden:tab-exited`
        // for one answer while unloading the other; they cannot disagree today, and collapsing to
        // the registry's `(tab_id, which)` as the single answer is what keeps it that way. One
        // lock is held across both, so the window set can't change between them either.
        if let Some((label, _, _)) = lock.locate_surface(surface_id) {
            let Some(ws) = lock.windows.get_mut(&label) else {
                return;
            };
            let Some((tab_id, which)) = ws.registry.locate_surface(surface_id) else {
                return;
            };
            let new_active = match which {
                crate::registry::PaneIdx::Secondary => {
                    ws.registry.close_secondary(&tab_id);
                    drop(lock);
                    // `warden:tab-exited` means "the whole tab went cold" to the chrome
                    // (`applyUnloaded`) — wrong here, since the tab and its primary are still
                    // live. `warden:pane-closed` is the narrower signal: it tells the chrome to
                    // collapse the split (`setSplitVisible(false)`) for exactly this tab, mirroring
                    // what the divider ✕ (`close_pane`) already does client-side, without touching
                    // the tab-level dot/highlight.
                    let _ = app.emit_to(
                        label.as_str(),
                        "warden:pane-closed",
                        serde_json::json!({ "label": label, "id": tab_id }),
                    );
                    return;
                }
                crate::registry::PaneIdx::Primary => ws.registry.unload(&tab_id),
            };
            drop(lock);
            // Per-window event: `emit_to` leaks to sibling webviews, so stamp the label and let the
            // chrome filter (see CLAUDE.md).
            let _ = app.emit_to(
                label.as_str(),
                "warden:tab-exited",
                serde_json::json!({ "label": label, "id": tab_id, "newActive": new_active }),
            );
            return;
        }

        // Not docked anywhere: a popped-out tab's surface, held by a detached window's entry.
        let Some((dlabel, which)) = lock.locate_detached_surface(surface_id) else {
            return; // already gone (redocked or closed) — drop the signal
        };
        match which {
            crate::registry::PaneIdx::Secondary => {
                let Some((origin_label, tab_id)) = lock.close_detached_secondary(&dlabel) else {
                    return;
                };
                drop(lock);
                announce_detached_secondary_closed(app, &dlabel, &origin_label, &tab_id);
            }
            crate::registry::PaneIdx::Primary => {
                let DetachedSurface {
                    surface,
                    secondary,
                    origin_label,
                    tab_id,
                } = lock.detached.remove(&dlabel).expect("located above");
                // The whole tab ends, scratch pane included — the same one-dead-tab semantic the
                // docked primary path applies via `unload`.
                close_both(surface, secondary);
                let new_active = lock
                    .windows
                    .get_mut(&origin_label)
                    .and_then(|ws| ws.registry.clear_detached(&tab_id));
                // The entry is gone from `detached`, so `is_empty` may now be true (origin closed
                // while the tab was out): the home surface must appear, exactly as it would after
                // `redock`. The window's own `Destroyed` → `redock` finds no entry and is a no-op.
                lock.sync_empty_surface(app);
                let dto = lock.init_dto(&origin_label);
                drop(lock);
                if let Some(win) = app.get_window(&dlabel) {
                    let _ = win.close();
                }
                // Snapshot first (the row stops rendering detached, reads cold), then the same
                // tail a docked exit runs.
                if let Some(dto) = dto {
                    let _ = app.emit_to(origin_label.as_str(), "warden:refresh", dto);
                }
                let _ = app.emit_to(
                    origin_label.as_str(),
                    "warden:tab-exited",
                    serde_json::json!({ "label": origin_label, "id": tab_id, "newActive": new_active }),
                );
            }
        }
    }

    /// Record that the surface `surface_id` became first responder — the user clicked into it, or
    /// `activate` focused it — as the focused pane of its tab (`Registry::focus_pane`), and tell
    /// that window's chrome (`warden:pane-focused`) so the focused-pane marker follows. This is
    /// the one route that catches a click on LIVE terminal content: the native `NSView` sits above
    /// the webview and consumes the click, so the chrome's own per-pane `mousedown` (which drives
    /// `focus_pane` for a cold pane's backstop) never fires for it. Both routes land on the same
    /// registry record. A surface in a detached window drives that page's own ring instead, via
    /// shell-core's `set_focused_hole` — the popped-out tab shows the same marker it did docked.
    pub fn handle_surface_focused(app: &AppHandle, surface_id: usize) {
        let state = app.state::<ManagerState>();
        let mut lock = state.lock();
        let Some((label, _, _)) = lock.locate_surface(surface_id) else {
            let Some((dlabel, which)) = lock.locate_detached_surface(surface_id) else {
                return;
            };
            drop(lock);
            let _ = shell_core::detach::set_focused_hole(app, &dlabel, which.index());
            return;
        };
        let Some(ws) = lock.windows.get_mut(&label) else {
            return;
        };
        let Some((tab_id, which)) = ws.registry.locate_surface(surface_id) else {
            return;
        };
        if !ws.registry.focus_pane(&tab_id, which) {
            return;
        }
        drop(lock);
        let _ = app.emit_to(
            label.as_str(),
            "warden:pane-focused",
            serde_json::json!({ "label": label, "id": tab_id, "pane": which.index() }),
        );
    }

    /// Retire a popped-out tab's second pane: close its surface and forget it. Returns the
    /// `(origin_label, tab_id)` the caller announces with [`announce_detached_secondary_closed`]
    /// once the lock is released (it emits). `None` if `dlabel` isn't a detached window or has
    /// no live secondary. Two triggers land here — the ✕ on the detached page's divider
    /// (`close_hole`) and the scratch shell exiting on its own — one teardown.
    pub fn close_detached_secondary(&mut self, dlabel: &str) -> Option<(String, String)> {
        let ds = self.detached.get_mut(dlabel)?;
        let s = ds.secondary.take()?;
        s.close();
        Some((ds.origin_label.clone(), ds.tab_id.clone()))
    }

    /// Which detached window (by label) holds the surface `surface_id`, and as which pane —
    /// the `detached` counterpart of `locate_surface`, for signals from a popped-out tab.
    pub fn locate_detached_surface(
        &self,
        surface_id: usize,
    ) -> Option<(String, crate::registry::PaneIdx)> {
        self.detached.iter().find_map(|(label, ds)| {
            if ds.surface.id() == surface_id {
                Some((label.clone(), crate::registry::PaneIdx::Primary))
            } else if ds.secondary.as_ref().is_some_and(|s| s.id() == surface_id) {
                Some((label.clone(), crate::registry::PaneIdx::Secondary))
            } else {
                None
            }
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
        // A popped-out (detached) window is a real surface on screen: while one exists the
        // app is NOT empty, so the home surface must not appear. `sync_empty_surface` reads
        // this from places (e.g. a real window's Destroyed handler) that don't otherwise know
        // about detached windows.
        self.windows.is_empty() && self.detached.is_empty()
    }

    /// The detached window's label for the tab `(origin_label, tab_id)`, if that tab is
    /// currently popped out. Used to route a click on a detached sidebar row (in the origin
    /// window) to "raise the popped-out window".
    pub fn detached_label_for(&self, origin_label: &str, tab_id: &str) -> Option<String> {
        self.detached
            .iter()
            .find(|(_, ds)| ds.origin_label == origin_label && ds.tab_id == tab_id)
            .map(|(label, _)| label.clone())
    }

    /// Update a detached window's surface frame — its content hole, reported by
    /// `detach.html`'s own `set_hole_rect` on load/resize (the detached window is not in
    /// `windows`, so the ordinary registry path can't reach it). No-op if `label` isn't a
    /// live detached window.
    ///
    /// `which` names the hole the rect came from, mirroring the ordinary registry path's
    /// per-pane routing. A single-hole detached window's page omits `pane` entirely, which
    /// `set_hole_rect` reads as `Primary` — so an unsplit pop-out lands here exactly as it
    /// did before there were panes at all. A `Secondary` rect for a window that carries no
    /// second surface is dropped rather than applied to the primary: it can only mean the
    /// page and this map disagree about how many panes there are, and re-pointing the one
    /// live surface at the other hole would move the terminal the user is typing in.
    pub fn set_detached_frame(
        &mut self,
        label: &str,
        which: crate::registry::PaneIdx,
        rect: PixelRect,
    ) {
        let Some(ds) = self.detached.get_mut(label) else {
            return;
        };
        match which {
            crate::registry::PaneIdx::Primary => ds.surface.set_frame(rect),
            crate::registry::PaneIdx::Secondary => {
                if let Some(s) = ds.secondary.as_ref() {
                    s.set_frame(rect);
                }
            }
        }
    }

    /// Return a popped-out tab's surface to its origin window when the detached window closes
    /// (`shell_core::detach::wire_return`'s `on_close`). Runs on the main thread under the
    /// `ManagerState` lock. Returns the origin label so the caller can rebuild the menu and
    /// push a refresh to it; `None` if the detached window was already gone (double-close).
    ///
    /// Edge cases, in order:
    /// 1. **Origin still open** — its slot is the `Detached` placeholder; `unload` is a no-op
    ///    on it, `reparent` moves the view back, `attach` (Detached → Spawned) restores it.
    /// 2. **Origin closed by the user while detached** — `reopen_window` rebuilds it from
    ///    config (which may spawn a fresh surface for this tab); `unload` kills that fresh
    ///    surface back to `Cold` so `attach` (Cold → Spawned) lands the RETURNING surface,
    ///    never overwriting a live one.
    /// 3. **Origin removed from config entirely** — the tab has no home, so the surface is
    ///    dropped (kills its PTY): the one place a live surface is intentionally dropped.
    ///
    /// Every step treats the pair as one unit: both surfaces reparent into the origin, both
    /// go back through one `attach`, and case 3 drops both. A split that returns to a window
    /// case 2 rebuilt *unsplit* keeps its second pane — `attach` recreates the slot rather
    /// than refusing (see its doc), so the second PTY survives the round trip too.
    pub fn redock(&mut self, app: &AppHandle, detached_label: &str) -> Option<String> {
        // App is quitting (⌘Q, `RunEvent::ExitRequested` fires before every window's
        // `Destroyed`): don't reopen an origin window or reparent a surface mid-teardown —
        // everything is being torn down and `GhosttySurface`'s `Drop` frees it. Without this,
        // a detached window whose origin was already closed would build a fresh window during
        // app termination (reaped by native `terminate:`, but real window resurrection).
        if is_quitting() {
            return None;
        }

        let DetachedSurface {
            mut surface,
            mut secondary,
            origin_label,
            tab_id,
        } = self.detached.remove(detached_label)?;

        // Rebuild the origin window if the user closed it while the tab was popped out, so the
        // tab has somewhere to return to (case 2).
        if !self.windows.contains_key(&origin_label) {
            self.reopen_window(app, &origin_label);
        }

        match self.windows.get_mut(&origin_label) {
            // Case 3: origin gone from config — the tab genuinely ends. Closing the surfaces
            // ends their PTYs (the only intentional live-surface teardown in the whole flow).
            None => close_both(surface, secondary),
            Some(ws) => {
                // Kill any fresh surface a reopen spawned for this tab (→ Cold) so `attach`
                // never overwrites a `Spawned` slot; on the origin-stayed-open path the slot
                // is `Detached` and this is a no-op. Acts on BOTH panes (see `unload`).
                ws.registry.unload(&tab_id);
                if let Ok(nsw) = ws.window.ns_window() {
                    // reparent only errors before it moves the view, so a failure here leaves
                    // the surface intact for `attach` below to re-home in the registry. Both
                    // panes come back — leaving the secondary parented to the window that is
                    // being destroyed would strand a live PTY with no view on screen.
                    let _ = surface.reparent(nsw as *mut c_void, INITIAL_RECT);
                    if let Some(s) = secondary.as_mut() {
                        let _ = s.reparent(nsw as *mut c_void, INITIAL_RECT);
                    }
                }
                match ws.registry.attach(&tab_id, surface, secondary) {
                    // Re-assert the origin's CURRENT selection — NOT the returned tab. The chrome
                    // owns selection (warden passes no `active`), so force-activating the returned
                    // tab here would show its terminal while the sidebar still highlights whatever
                    // was selected: the shown surface and the sidebar diverge, and re-popping the
                    // now-shown-but-unselected tab leaves an uncovered (transparent) hole.
                    // `reparent` unhid the returning surface; re-activating the real selection
                    // re-hides it (activate hides all others). If nothing is active (the selection
                    // was closed while this tab was out), the returned tab becomes active — there
                    // is nothing else to show.
                    Ok(()) => {
                        let show = ws
                            .registry
                            .active_tab()
                            .map(str::to_string)
                            .unwrap_or_else(|| tab_id.clone());
                        let _ = ws.registry.activate(&show);
                    }
                    // Defensive: a slot wasn't Cold/Detached (shouldn't happen — `unload`
                    // above just cleared both) — take the hand-back and close it rather than
                    // leak, keeping the decision explicit. `attach` is all-or-nothing, so
                    // this is both surfaces or neither, never a half-restored tab.
                    Err((p, s)) => close_both(p, s),
                }
            }
        }

        self.sync_empty_surface(app);
        Some(origin_label)
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
                        // Add/remove/respawn tabs, skipping any that are currently
                        // popped out (Detached) — see `apply_tab_reconcile`.
                        apply_tab_reconcile(
                            &mut ws.registry,
                            &remove_tabs,
                            &add_tabs,
                            &respawn_tabs,
                        );
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

/// Apply a hot-reload's tab-set reconcile ops (from the config diff) to one
/// window's registry — **skipping any tab currently popped out (`Detached`)**.
///
/// `reconcile_ops` is derived purely from the config diff and knows nothing about
/// runtime pop-out state, so a config edit to a currently-detached tab would
/// otherwise clobber its placeholder: `remove`/`respawn`'s `remove` drops the
/// slot the live surface must return to (→ `redock` finds no slot and `drop`s the
/// PTY — silent data loss), and `add`/`respawn`'s `add` eagerly spawns a duplicate
/// surface for a tab already live elsewhere. Guarding each op on `is_detached`
/// leaves `redock` the sole owner of a detached tab's lifecycle; the new spec is
/// re-applied by the reconcile that runs once the tab is docked home again.
///
/// The skip keys on `Tab::key` (`id`-else-normalized-`dir`) exactly as the
/// registry keys every entry — `remove_tabs` carries those keys directly, and a
/// `TabPlan`'s `spec.id` *is* that key. A non-detached tab reconciles unchanged.
fn apply_tab_reconcile(
    registry: &mut Registry,
    remove_tabs: &[String],
    add_tabs: &[TabPlan],
    respawn_tabs: &[TabPlan],
) {
    for id in remove_tabs {
        // A popped-out tab's placeholder is redock's to manage — dropping it here
        // strands the live surface with no slot to return to.
        if registry.is_detached(id) {
            continue;
        }
        registry.remove(id);
    }
    for tp in add_tabs {
        // An add for a currently-detached key would spawn a duplicate PTY for a
        // tab the placeholder already represents. (An add normally only fires for a
        // genuinely new key; this guard is defensive.)
        if registry.is_detached(&tp.spec.id) {
            continue;
        }
        // A failed eager spawn on hot-reload leaves the tab cold (it retries on
        // focus, which surfaces the error via the banner then) — log it, never panic.
        if let Err(e) = registry.add(&tp.spec, tp.load_on_open) {
            eprintln!(
                "warden: surface spawn failed for tab {:?}: {e}",
                tp.spec.title
            );
        }
    }
    // Respawn kept tabs whose terminal spec changed: tear down and rebuild by the
    // same id (identity is stable). A cold tab just gets a fresh spec and lazy-spawns
    // on next focus; a load_on_open tab respawns eagerly. The previously-active tab is
    // re-activated by the caller so a visible respawn shows its new surface immediately.
    for tp in respawn_tabs {
        // A detached tab is out on loan: its surface lives in the popped-out
        // window's registry, so tearing down the placeholder here would clobber the
        // live PTY. Skip — the new spec is picked up by the reconcile that runs once
        // it's docked home.
        if registry.is_detached(&tp.spec.id) {
            continue;
        }
        registry.remove(&tp.spec.id);
        if let Err(e) = registry.add(&tp.spec, tp.load_on_open) {
            eprintln!(
                "warden: surface respawn failed for tab {:?}: {e}",
                tp.spec.title
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::TabSpec;
    use std::sync::atomic::Ordering;

    #[test]
    fn probe_interval_defaults_to_5_and_is_settable() {
        let m = WindowManager::new();
        assert_eq!(m.probe_interval.load(Ordering::Relaxed), 5);
        m.set_probe_interval(0);
        assert_eq!(m.probe_interval.load(Ordering::Relaxed), 0);
    }

    fn tab_spec(id: &str, dir: &str) -> TabSpec {
        TabSpec {
            id: id.into(),
            title: id.into(),
            dir: std::path::PathBuf::from(dir),
            shell: "sh".into(),
            startup: None,
            group: None,
            probe: None,
            kill: None,
            tree: false,
            tree_path: Vec::new(),
        }
    }

    // `apply` itself needs a live `AppHandle` (Tauri) and a real `GhosttySurface`
    // (AppKit) — both unconstructable in a unit-test process — so the detached-aware
    // guard is exercised at its narrowest real layer: `apply_tab_reconcile`, the free
    // function `apply`'s `WindowOp::Update` arm delegates the add/remove/respawn to.
    // `force_detached` reaches a `Detached` slot without a live surface (see registry.rs).
    #[test]
    fn reconcile_leaves_a_detached_tabs_placeholder_alone() {
        // A hot-reload whose config diff removes/respawns a currently popped-out tab
        // must NOT touch its Detached placeholder: the live surface lives in the
        // popped-out window's registry and `redock` is its sole owner. Clobbering the
        // placeholder either strands the surface (remove → redock drops the PTY, silent
        // data loss) or spawns a duplicate (respawn's/ add's `add`).
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/old"), false);
        let _ = r.add(&tab_spec("t1", "/tmp/b"), false);
        r.force_detached("t0");

        // Respawn variant: config changed t0's dir (id stable) → respawn_tabs=[t0]. A
        // non-detached sibling t1 is genuinely removed to prove the normal path is intact.
        let respawn = vec![TabPlan {
            spec: tab_spec("t0", "/tmp/new"),
            load_on_open: false,
        }];
        apply_tab_reconcile(&mut r, &["t1".to_string()], &[], &respawn);
        assert!(
            r.is_detached("t0"),
            "detached tab's placeholder must survive a respawn op"
        );
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec!["t0".to_string()],
            "t0 kept (detached), t1 removed via the normal path, no duplicate entry"
        );
        assert!(
            !r.is_spawned("t0"),
            "no duplicate surface spawned for the detached tab"
        );

        // Remove variant: config deleted t0 while it's out → remove_tabs=[t0].
        apply_tab_reconcile(&mut r, &["t0".to_string()], &[], &[]);
        assert!(
            r.is_detached("t0"),
            "detached tab's placeholder must survive a remove op (returns on redock)"
        );
        assert_eq!(r.tab_dtos().len(), 1, "still present, not dropped");

        // Add variant (defensive): an add for a currently-detached key must not append
        // a duplicate entry — the placeholder already represents it.
        apply_tab_reconcile(
            &mut r,
            &[],
            &[TabPlan {
                spec: tab_spec("t0", "/tmp/x"),
                load_on_open: false,
            }],
            &[],
        );
        assert!(r.is_detached("t0"));
        assert_eq!(
            r.tab_dtos().len(),
            1,
            "add for a detached key must not create a duplicate entry"
        );
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
            secondary_spawned: false,
            split: false,
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
