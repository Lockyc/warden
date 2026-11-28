//! Owns each open window's content: its terminals (a `Registry` per window label, a pop-out's
//! among them), the held configs and remembered tabs. Which windows exist and under which label is
//! shell-core's `WindowSet`'s, which builds and tears down this content through `WardenHost`
//! (`windows.rs`). Impure (Tauri + AppKit) — verified at checkpoints.

use crate::plan::{tab_ops, window_to_spec, TabPlan, WindowSpec};
use crate::probe::Presence;
use crate::registry::{ProbeTarget, Registry, TabDto};
use crate::surface::ghostty::GhosttySurface;
use crate::surface::{PixelRect, TabSpec};
use crate::ManagerState;
use shell_core::spaces::ShellWindow as _;
use shell_core::windows::WindowSet;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use warden_config::{Colour, Config, Reconciliation, SpecChange};

/// Initial surface rect: offset by the 160px sidebar so the surface never
/// overlaps it before the first JS rect report arrives. (Matches the spike.)
pub(crate) const INITIAL_RECT: PixelRect = PixelRect {
    x: 160.0,
    y: 0.0,
    width: 740.0,
    height: 600.0,
};

/// Whether the app is quitting: `RunEvent::ExitRequested` (main.rs) marks the `WindowSet`, which
/// fires before every window's `Destroyed` during ⌘Q.
pub fn is_quitting(app: &AppHandle) -> bool {
    app.try_state::<WindowSet>()
        .is_some_and(|set| set.is_quitting())
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
/// This is what the whole pipeline (window_to_spec / reconcile / registry) consumes, so a
/// discovered project is just a `Tab` and needs no special-casing downstream. Roots'
/// tabs come after loose + grouped tabs, matching the "sections in file order" rule. With the
/// Rex hub, each window then lists the unclaimed sessions on its Rex tabs' servers
/// ([`crate::rex::unclaimed::append`]) — lock-only reads, no network.
pub fn effective_config(config: &Config, rex: Option<&crate::rex::RexHub>) -> Config {
    let mut eff = config.clone();
    for window in &mut eff.windows {
        // A discovered project's key is its normalized dir. Seeding `seen` with every curated
        // tab's normalized dir as well as its key makes a curated tab shadow the project at its
        // dir whether or not it carries an `id`; the first-occurrence dedup then also lands a
        // project two overlapping roots both discover once. Curated tabs come first, so no
        // curated tab is ever the one dropped — no "same repo twice", and no key collision for
        // reconcile's find-first matching.
        let mut seen: HashSet<String> = window
            .tabs
            .iter()
            .map(|t| warden_config::normalize_dir_key(&t.dir))
            .collect();
        let mut curated_keys = HashSet::new();
        window.tabs.retain(|t| curated_keys.insert(t.key.clone()));
        seen.extend(curated_keys);
        for root in &window.roots {
            for t in crate::scanner::synthesize_tabs(root) {
                if seen.insert(t.key.clone()) {
                    window.tabs.push(t);
                }
            }
        }
    }
    if let Some(hub) = rex {
        crate::rex::unclaimed::append(&mut eff, hub);
    }
    eff
}

const SPLIT_BAND: [f64; 2] = [
    warden_config::resolve::SPLIT_SIZE_MIN,
    warden_config::resolve::SPLIT_SIZE_MAX,
];

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
    /// Whether the sidebar pins a section of the currently-open tabs above the main
    /// list: this window's resolved `open_tabs_section` (global → window, default
    /// false), read from `WindowState`.
    pub open_tabs_section: bool,
    /// Whether the sidebar pins a section of the unloaded tabs with a recoverable
    /// session: this window's resolved `background_tabs_section` (default true).
    pub background_tabs_section: bool,
    /// Whether warden checks for a new release on launch, from the global
    /// `auto_update` config (default true). Carried per-window (it's global); the
    /// chrome gates its launch-time update check on it (the menu check ignores it).
    pub auto_update: bool,
    /// `[SPLIT_SIZE_MIN, SPLIT_SIZE_MAX]`: the share either pane of a split may take. The
    /// chrome clamps its divider drag to it and rejects a saved ratio outside it.
    pub split_band: [f64; 2],
    /// `rex::layout::RATIO_BAND`: the ratio a scratch column divider may take within its split.
    /// The chrome clamps its drag to it; the backend clamps again on the way to Rex.
    pub scratch_band: [f64; 2],
    /// The terminal's font size in points (`ghostty::terminal_font_size`); the chrome's agent
    /// column and summary block size their text with it.
    pub terminal_font_size: f32,
    /// This window's `[[window.root]]` names, from `last_good`: chrome-core paints a
    /// root's section (head, ⟳ rescan, "No projects found") even when it found no projects.
    pub trees: Vec<String>,
    pub tabs: Vec<TabDto>,
    /// The registry's active tab. The chrome owns selection, so it reads this on init only —
    /// to open on the tab `build_window` activated (a `remember_tabs` restore) rather than
    /// falling back to the first tab, which would activate, and so spawn, that one too.
    pub active: Option<String>,
    /// A surface-spawn failure that happened while building this window, surfaced
    /// in the chrome's error banner on init. `None` = all tabs built cleanly. This
    /// is the launch channel for spawn errors: `build_window` runs before the
    /// webview registers its `warden:error` listener, so a pushed event would be
    /// lost — the chrome pulls this with the snapshot instead.
    pub error: Option<String>,
    /// This window's tabs with an unread bell badge (`unread.rs`). The chrome seeds its badges
    /// from this on init only; within a run `warden:notify` and `clear_unread` keep them.
    pub unread: Vec<String>,
}

/// Where a pop-out's tab came from: the window it returns to — found by `label` while that
/// window is open, else by `title`, which follows a rename in place (`retitle`) and a rebuild
/// under another label (`build_window`) — and the tab's id there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopOrigin {
    pub label: String,
    pub title: String,
    pub tab_id: String,
}

/// The sidebar rows `(window label, tab id)` showing tab `tab` of window `label`: that row, and
/// for a pop-out (its `origin`) the origin's placeholder row, which stands for the tab while it is
/// out. A badge, a seen and a presence dot for the tab go to each.
pub fn tab_rows<'a>(
    label: &'a str,
    tab: &'a str,
    origin: Option<&'a PopOrigin>,
) -> Vec<(&'a str, &'a str)> {
    let mut rows = vec![(label, tab)];
    rows.extend(origin.map(|o| (o.label.as_str(), o.tab_id.as_str())));
    rows
}

/// Where a pop-out's tab goes home to ([`return_target`]).
#[derive(Debug, PartialEq, Eq)]
pub enum ReturnTarget {
    /// The open window under this label.
    Open(String),
    /// No window is open with the origin's title: reopen the window with this title first.
    Reopen(String),
}

/// Where a pop-out's tab returns, given the open windows as `(label, title)`: the origin's own
/// label while a window under it still has the origin's title; else the window now carrying that
/// title (renamed in place, or rebuilt under another label); else reopen it by title. A window
/// that took the origin's label under another title is not its home.
pub fn return_target(origin: &PopOrigin, open: &[(&str, &str)]) -> ReturnTarget {
    let titled = |t: &str| same_title(t, &origin.title);
    if open.iter().any(|(l, t)| *l == origin.label && titled(t)) {
        return ReturnTarget::Open(origin.label.clone());
    }
    match open.iter().find(|(_, t)| titled(t)) {
        Some((l, _)) => ReturnTarget::Open((*l).to_owned()),
        None => ReturnTarget::Reopen(origin.title.clone()),
    }
}

/// Whether two window titles name the same window: equal once trimmed, as `spec_for` and
/// `retitle` match them. The one rule every pop-out origin lookup uses.
fn same_title(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

/// Whether a pop-out has nothing left to show: its one tab is no longer up — no live surface, and
/// not an agent pane open on sleeping agents, which stays so they can be woken from its list. Its
/// window then closes and the tab goes home cold ([`close_spent_popouts`]).
pub fn popout_should_close(reg: &Registry) -> bool {
    !reg.sole_tab_up()
}

/// Close every pop-out with nothing left to show ([`popout_should_close`]) — run after a child
/// exit, an unload or an agent close, any of which can leave a pop-out's tab cold. Each window's
/// `Destroyed` then returns its tab (`main.rs::redock`). Locks `ManagerState`, so callers must
/// hold none.
pub fn close_spent_popouts(app: &AppHandle) {
    let spent: Vec<String> = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows
            .iter()
            .filter(|(l, ws)| WindowManager::is_popout(l) && popout_should_close(&ws.registry))
            .map(|(l, _)| l.clone())
            .collect()
    };
    for label in spent {
        log::info!("{label:?}: nothing left running, closing");
        if let Some(w) = app.get_window(&label) {
            let _ = w.close();
        }
    }
}

pub struct WindowState {
    pub registry: Registry,
    pub title: String,
    pub colour: String,
    /// Whether THIS window's sidebar pins the "Open" section — the cascade already
    /// collapsed by resolve. Stamped from the spec at build, updated in place by a
    /// hot-reload `Update` op, and read by `init_dto`/the refresh DTO. It lives here
    /// rather than being read off `last_good` per emit because it is per-window now:
    /// a global edit reaches a window as an ordinary reconcile diff.
    pub open_tabs_section: bool,
    /// Whether THIS window's sidebar pins the "Background" section — kept exactly as
    /// `open_tabs_section` is.
    pub background_tabs_section: bool,
    /// Surface-spawn failure(s) from `build_window`, shown once via the init DTO.
    pub spawn_error: Option<String>,
    /// The `SessionStore` key this window's remembered tabs live under: its launch label
    /// (`WindowSet::launch_label`), which a rename in place moves (`retitle`). Empty for a
    /// pop-out, which remembers nothing.
    pub session_key: String,
    /// Whether this window's chrome has pulled its init snapshot (`init_tabs`), so an emitted
    /// `warden:focus-tab` reaches a listener.
    pub chrome_ready: bool,
    /// The tab a `warden://open?…&tab=` named before the chrome mounted; `init_tabs` hands it
    /// over as the tab to open on.
    pub pending_focus: Option<String>,
    /// A pop-out's origin; `None` for a configured window. A pop-out is a one-tab window under a
    /// `shell-detach:` label whose registry holds the tab moved out of the origin, where a
    /// placeholder keeps its row ([`Registry::take_tab`]).
    pub origin: Option<PopOrigin>,
}

/// The id of the tab `name` names among `tabs` (`(id, title)` pairs): an exact id, else the
/// first tab titled `name`.
pub fn tab_named<'a>(tabs: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    tabs.iter()
        .find(|(id, _)| *id == name)
        .or_else(|| tabs.iter().find(|(_, title)| *title == name))
        .map(|(id, _)| *id)
}

/// Tell window `label`'s chrome that tab `tab_id`'s scratch column is gone — a plain tab's one
/// pane exited or was closed, a Rex column's session ended — so it collapses the split for exactly
/// that tab. Not `warden:tab-exited`,
/// which means the whole tab went cold (`applyUnloaded`): the tab and its primary are still live.
pub fn announce_pane_closed(app: &AppHandle, label: &str, tab_id: &str) {
    let _ = app.emit_to(
        label,
        "warden:pane-closed",
        serde_json::json!({ "label": label, "id": tab_id }),
    );
}

/// What `WindowManager::redock` hands its caller to announce with the lock released.
pub struct Redocked {
    pub origin_label: String,
    /// `(tab_id, new_active)` when the returning tab came home with nothing up, or was dropped
    /// (it left the config while out): the chrome gets the same `warden:tab-exited` an unload
    /// sends, so its selection follows the neighbour the registry leaned to.
    pub dropped: Option<(String, Option<String>)>,
}

pub struct WindowManager {
    pub windows: HashMap<String, WindowState>, // key = Tauri label (the `WindowSet`'s)
    pub last_good: Config,
    /// The RAW config as loaded (roots UNexpanded). `last_good` is its expanded
    /// form (`effective_config(&raw_config, hub)`, i.e. with discovered project tabs
    /// appended); `raw_config` is retained so `rescan_root` can re-expand + re-scan
    /// without a config-file change.
    pub raw_config: Config,
    /// The presence scheduler's slow-floor cadence in seconds; shared as an `Arc` with
    /// `probe::run_scheduler` so a hot-reload changes it live. 0 = event-driven only
    /// (burst on triggers, then Idle — no steady polling between events).
    pub probe_interval: Arc<AtomicU64>,
    /// Last-known session presence per tab, surviving window close/reopen so a (re)opened
    /// window's `init_dto`/refresh paints its cyan dots from the initial snapshot instead of
    /// waiting for the first post-boot probe emit (see `PresenceCache`). Written by the
    /// scheduler each probe pass (`probe::probe_window`), read when building DTOs.
    pub presence_cache: PresenceCache,
    /// Titles of windows that closed, most-recent last (MRU stack). Drives `⌘⇧T` (Reopen Last
    /// Closed). Deduped on push so a repeated close moves the title to the end; a window's build
    /// drops its title, and a rename in place carries it. Titles since deleted from the config
    /// are skipped at reopen time (`plan::next_reopen_target`).
    pub last_closed: Vec<String>,
    /// Each window's remembered loaded + active tabs (`remember_tabs`): read by `build_window`,
    /// written by `record_session`. In-memory only until `main.rs`'s setup points it at disk.
    pub session: crate::session::SessionStore,
    /// Unread bell badges, under [`Self::unread_key`]. In-memory only until `main.rs`'s setup
    /// points it at disk.
    pub unread: crate::unread::UnreadStore,
}

impl WindowManager {
    pub fn new() -> Self {
        // `last_good` (effective) and `raw_config` (raw) start identical and empty, at
        // warden-config's own defaults.
        let empty = Config::empty();
        let probe_interval = Arc::new(AtomicU64::new(empty.probe_interval));
        WindowManager {
            windows: HashMap::new(),
            raw_config: empty.clone(),
            last_good: empty,
            probe_interval,
            presence_cache: PresenceCache::default(),
            last_closed: Vec::new(),
            session: Default::default(),
            unread: Default::default(),
        }
    }

    /// Mirror the pop-out count into the `WindowSet`, which keeps the home surface away while a
    /// pop-out is the only window on screen. Call after a pop-out opens or closes.
    pub fn sync_popouts(&self, app: &AppHandle) {
        if let Some(set) = app.try_state::<WindowSet>() {
            let n = self.windows.keys().filter(|l| Self::is_popout(l)).count();
            set.set_detached_count(app, n);
        }
    }

    /// Whether `label` is a pop-out's: a one-tab window under a `shell-detach:` label.
    pub fn is_popout(label: &str) -> bool {
        shell_core::detach::is_detached_label(label)
    }

    /// The pop-out holding tab `tab_id` popped out of window `origin_label`, if it is out.
    pub fn popout_of(&self, origin_label: &str, tab_id: &str) -> Option<&str> {
        self.windows.iter().find_map(|(l, ws)| {
            let o = ws.origin.as_ref()?;
            (o.label == origin_label && o.tab_id == tab_id).then_some(l.as_str())
        })
    }

    /// The window holding tab `tab_id` itself — a pop-out holding it, never a window with only its
    /// placeholder — for a reader that knows the tab but not the window.
    pub fn home_of(&self, tab_id: &str) -> Option<&str> {
        self.windows
            .iter()
            .find(|(_, ws)| ws.registry.holds_tab(tab_id))
            .map(|(l, _)| l.as_str())
    }

    /// The config window title `label`'s tabs live under: its own, or for a pop-out its origin's.
    pub fn config_title(&self, label: &str) -> Option<&str> {
        let ws = self.windows.get(label)?;
        Some(ws.origin.as_ref().map_or(&ws.title, |o| &o.title))
    }

    /// Pop-out `label`'s origin among the open configured windows ([`return_target`]); `None`
    /// when `label` is no pop-out.
    pub fn origin_home(&self, label: &str) -> Option<ReturnTarget> {
        let origin = self.windows.get(label)?.origin.as_ref()?;
        let open: Vec<(&str, &str)> = self
            .windows
            .iter()
            .filter(|(_, ws)| ws.origin.is_none())
            .map(|(l, ws)| (l.as_str(), ws.title.as_str()))
            .collect();
        Some(return_target(origin, &open))
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

    /// The spec for the configured window titled `title`, under `label` — from `last_good`, so it
    /// carries the discovered project tabs.
    pub fn spec_for(&self, label: &str, title: &str) -> Option<WindowSpec> {
        self.last_good
            .windows
            .iter()
            .find(|w| w.title == title.trim())
            .map(|w| window_to_spec(w, label.to_string()))
    }

    /// Build one Tauri window for `spec`, mount its tabs, activate the first, and register it
    /// under `spec.label` with its remembered tabs keyed `session_key`. `WardenHost::build` —
    /// the `WindowSet` decides when, and wires the window's close.
    pub fn build_window(
        &mut self,
        app: &AppHandle,
        spec: &WindowSpec,
        session_key: String,
    ) -> tauri::Result<WebviewWindow> {
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
                .shell_window()
                .build()?;
        log::info!(
            "window {:?} opened ({} tab(s))",
            spec.title,
            spec.tabs.len()
        );

        // FOOTGUN: do NOT restore geometry by hand here. This window is built at runtime, not
        // from `tauri.conf.json`, so a manual restore looks needed — but shell-core's geometry
        // plugin restores saved bounds on its `on_window_ready` hook, which fires for
        // runtime-built windows too. A second restore here double-applies them.

        let ns_window = window.ns_window()? as *mut std::os::raw::c_void;

        let mut registry = Registry::new(ns_window, INITIAL_RECT);
        registry.set_rex_opener(crate::rex::spawn::opener(app.clone(), spec.label.clone()));
        registry.set_report_socket(crate::rex::reports::socket_path(app));
        // Surface-create failures (a null libghostty surface, an interior-NUL in a
        // config dir/shell) must NOT panic the whole app at launch — the failing
        // tab stays cold and its reason is collected for the init banner; every
        // other tab and window still comes up.
        let mut spawn_errors: Vec<String> = Vec::new();
        let restore = spec
            .remember_tabs
            .then(|| self.session.get(&session_key))
            .flatten()
            .map(|r| r.restore(spec.tabs.iter().map(|t| t.spec.id.as_str())))
            .unwrap_or_default();
        // A tab still popped out of this window (rebuilt while it's out) has its terminals in the
        // pop-out: its entry is added cold and then made the placeholder `take_tab` leaves, so
        // nothing here spawns a second one and the tab has its slot to come home to. Matched by
        // title: the window may come back under another label than the one it had.
        let out: Vec<String> = self
            .windows
            .values_mut()
            .filter_map(|ws| ws.origin.as_mut())
            .filter(|o| same_title(&o.title, &spec.title))
            .map(|o| {
                o.label = spec.label.clone();
                o.tab_id.clone()
            })
            .collect();
        for t in &spec.tabs {
            let id = t.spec.id.as_str();
            let eager =
                (t.load_on_open || restore.loaded.contains(id)) && !out.iter().any(|o| o == id);
            if let Err(e) = registry.add(&t.spec, eager) {
                spawn_errors.push(format!("{}: {e}", t.spec.title));
            }
        }
        for id in &out {
            registry.stand_placeholder(id);
        }
        // The first tab shown is one with a terminal to show here — never a placeholder, whose
        // hole in this window would sit empty.
        let active = restore
            .active
            .as_deref()
            .and_then(|id| spec.tabs.iter().find(|t| t.spec.id == id))
            .into_iter()
            .chain(&spec.tabs)
            .find(|t| !registry.is_placeholder(&t.spec.id));
        if let Some(first) = active {
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
            log::error!("surface spawn failed in window {:?}: {joined}", spec.title);
            Some(format!("couldn't open terminal — {joined}"))
        };

        // Focus → fast-burst this window's session dots so a just-focused window is current
        // within a burst pass (covers `probe_interval = 0`, which never steady-polls), and clear
        // the badge its active docked tab took while the window was unfocused (`notify::raise`).
        // Its close is the `WindowSet`'s, which calls `WardenHost::closed`.
        let app_for_event = app.clone();
        let label_for_event = spec.label.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::Focused(true) = event {
                crate::probe::bump(&label_for_event);
                let seen = app_for_event
                    .try_state::<crate::ManagerState>()
                    .and_then(|st| {
                        let m = st.lock();
                        let reg = &m.windows.get(&label_for_event)?.registry;
                        let tab = reg.active_tab()?;
                        (!reg.is_placeholder(tab)).then(|| tab.to_owned())
                    });
                if let Some(tab) = seen {
                    let _ = app_for_event.emit_to(
                        label_for_event.as_str(),
                        "warden:tab-seen",
                        serde_json::json!({ "label": label_for_event, "id": tab }),
                    );
                }
                crate::sync_agent_menu(&app_for_event);
            }
        });

        self.last_closed.retain(|t| *t != spec.title);
        self.windows.insert(
            spec.label.clone(),
            WindowState {
                registry,
                title: spec.title.clone(),
                colour: spec.colour.clone(),
                open_tabs_section: spec.open_tabs_section,
                background_tabs_section: spec.background_tabs_section,
                spawn_error,
                session_key,
                chrome_ready: false,
                pending_focus: None,
                origin: None,
            },
        );
        Ok(window)
    }

    /// Register pop-out `label`: a one-tab window whose `registry` already holds the tab moved
    /// out of `origin`, shown under the tab's `title` in the origin's `colour`. It remembers no
    /// tabs and carries no sections; its focus clears the badge its tab took while unfocused, on
    /// its own row and the origin's placeholder ([`tab_rows`]), and refreshes the menu.
    pub fn add_popout(
        &mut self,
        app: &AppHandle,
        label: &str,
        registry: Registry,
        title: String,
        colour: String,
        origin: PopOrigin,
    ) {
        if let Some(window) = app.get_window(label) {
            let (app, label) = (app.clone(), label.to_owned());
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Focused(true) = event {
                    crate::probe::bump(&label);
                    let seen = app.try_state::<crate::ManagerState>().and_then(|st| {
                        let m = st.lock();
                        let ws = m.windows.get(&label)?;
                        let tab = ws.registry.active_tab()?;
                        Some(
                            tab_rows(&label, tab, ws.origin.as_ref())
                                .into_iter()
                                .map(|(l, t)| (l.to_owned(), t.to_owned()))
                                .collect::<Vec<_>>(),
                        )
                    });
                    for (row_label, row_tab) in seen.unwrap_or_default() {
                        let _ = app.emit_to(
                            row_label.as_str(),
                            "warden:tab-seen",
                            serde_json::json!({ "label": row_label, "id": row_tab }),
                        );
                    }
                    crate::sync_agent_menu(&app);
                }
            });
        }
        self.windows.insert(
            label.to_owned(),
            WindowState {
                registry,
                title,
                colour,
                open_tabs_section: false,
                background_tabs_section: false,
                spawn_error: None,
                session_key: String::new(),
                chrome_ready: false,
                pending_focus: None,
                origin: Some(origin),
            },
        );
        self.sync_popouts(app);
    }

    /// `WardenHost::closed`: drop `label`'s state and reap its surfaces eagerly via `close_all`
    /// (rather than `GhosttySurface`'s `Drop` safety net), and remember its title for `⌘⇧T`.
    /// Fires for a user close and for a window the `WindowSet` closed (deleted, or gone from the
    /// config — the latter skipped at reopen time). Idempotent.
    pub fn closed(&mut self, label: &str) {
        if let Some(mut ws) = self.windows.remove(label) {
            log::info!("window {:?} closed", ws.title);
            ws.registry.close_all();
            self.last_closed.retain(|t| *t != ws.title);
            self.last_closed.push(ws.title);
        }
    }

    /// `WardenHost::retitled`: the window titled `old` was renamed to `title` or recoloured in
    /// place; `label` is the label shell-core reports — an open window's live one, which a rename
    /// keeps. An open window's terminals keep running; the banner, the held configs (so a lookup by
    /// title finds the window before the reload echo), `⌘⇧T`'s stack, its popped-out tabs' origin
    /// and its remembered-tabs record (moved to its new launch label) follow it, open or closed.
    pub fn retitle(&mut self, label: &str, old: &str, title: &str, colour: Colour) {
        let title = title.trim();
        let Some(idx) = self
            .last_good
            .windows
            .iter()
            .position(|w| w.title.trim() == old.trim())
        else {
            return;
        };
        let old = self.last_good.windows[idx].title.clone();
        let open = self.windows.get(label).filter(|ws| ws.title == old);
        let old_key = open
            .map(|ws| ws.session_key.clone())
            .or_else(|| crate::plan::launch_label(&self.last_good, idx));
        crate::plan::retitle_window(&mut self.last_good, &old, title, colour);
        crate::plan::retitle_window(&mut self.raw_config, &old, title, colour);
        let renamed = self.last_closed.iter_mut().chain(
            self.windows
                .values_mut()
                .filter_map(|ws| ws.origin.as_mut())
                .map(|o| &mut o.title),
        );
        for t in renamed.filter(|t| **t == old) {
            *t = title.to_string();
        }
        let new_key = crate::plan::launch_label(&self.last_good, idx);
        if let (Some(old_key), Some(new_key)) = (old_key, new_key) {
            if old_key != new_key {
                self.session.rekey(&old_key, &new_key);
                self.unread.rekey(&old_key, &new_key);
            }
            if let Some(ws) = self.windows.get_mut(label).filter(|ws| ws.title == old) {
                ws.session_key = new_key;
            }
        }
        if let Some(ws) = self.windows.get_mut(label).filter(|ws| ws.title == old) {
            ws.title = title.to_string();
            ws.colour = colour.hex();
        }
    }

    /// Adopt a freshly loaded config — `config` raw, `effective` its root-expanded form (see
    /// `effective_config`, which callers run *before* locking: the scan must never run under the
    /// lock, it would block the probe thread). `raw_config` keeps the unexpanded config for later
    /// re-scans (`rescan_root`), while `last_good` (the reconcile baseline) holds the expanded
    /// form so a reopened window still carries its discovered project tabs.
    pub fn adopt(&mut self, config: Config, effective: Config) {
        self.adopt_raw(config);
        self.last_good = effective;
    }

    /// The raw-config half of [`adopt`](Self::adopt): where a loaded config's global settings
    /// take effect. A hot-reload's reconcile branch calls it directly (its `apply` advances
    /// `last_good`), so a global applied here reaches launch, recovery and reload alike.
    pub fn adopt_raw(&mut self, config: Config) {
        self.set_probe_interval(config.probe_interval);
        self.raw_config = config;
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
                open_tabs_section: ws.open_tabs_section,
                background_tabs_section: ws.background_tabs_section,
                auto_update: self.last_good.auto_update,
                split_band: SPLIT_BAND,
                scratch_band: crate::rex::layout::RATIO_BAND,
                terminal_font_size: crate::surface::ghostty::terminal_font_size(),
                // A pop-out's one tab carries no root section of its own.
                trees: match ws.origin {
                    Some(_) => Vec::new(),
                    None => self
                        .last_good
                        .windows
                        .iter()
                        .find(|w| w.title == ws.title)
                        .map(warden_config::Window::tree_names)
                        .unwrap_or_default(),
                },
                active: ws.registry.active_tab().map(str::to_string),
                unread: tabs
                    .iter()
                    .filter(|t| {
                        self.unread_key(label, &t.id)
                            .is_some_and(|(k, tab)| self.unread.has(&k, &tab))
                    })
                    .map(|t| t.id.clone())
                    .collect(),
                tabs,
                error: ws.spawn_error.clone(),
            }
        })
    }

    /// Where tab `tab` of window `label` keeps its unread badge: the window's launch label, or
    /// for a pop-out its origin's (by title, as `return_target` finds a home) and the tab's id
    /// there. `None` for an unknown window or a pop-out whose origin left the config.
    pub fn unread_key(&self, label: &str, tab: &str) -> Option<(String, String)> {
        let ws = self.windows.get(label)?;
        match &ws.origin {
            None => Some((ws.session_key.clone(), tab.to_owned())),
            Some(o) => {
                let idx = self
                    .last_good
                    .windows
                    .iter()
                    .position(|w| same_title(&w.title, &o.title))?;
                let key = crate::plan::launch_label(&self.last_good, idx)?;
                Some((key, o.tab_id.clone()))
            }
        }
    }

    /// Mark tab `tab` of window `label` unread, or seen.
    pub fn set_unread(&mut self, label: &str, tab: &str, unread: bool) {
        let Some((key, tab)) = self.unread_key(label, tab) else {
            return;
        };
        if unread {
            self.unread.add(&key, &tab);
        } else {
            self.unread.clear(&key, &tab);
        }
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
    /// Child-exit is per-pane: a plain tab's column block exiting closes just that pane — the
    /// scratch shell is done, the tab and its agent are not. Unloading the whole tab here would
    /// kill a live agent because a shell beside it exited. A PRIMARY exit still
    /// emits `warden:tab-exited`; that event means "the whole tab went cold" to the chrome
    /// (`applyUnloaded`), which is wrong for a pane that merely closed — a block exit instead
    /// emits the narrower `warden:pane-closed`, so the chrome collapses the split without
    /// touching the tab's own dot/highlight. A Rex column's block leaves its slot cold and the
    /// column to follow what the hub finds (`Registry::release_block`): the session's next view
    /// drops the block, or its end drops the column with the same `warden:pane-closed`.
    ///
    /// A pop-out's surface takes the same path in the pop-out's own registry; a pop-out left with
    /// nothing up then closes, taking its tab home cold ([`close_spent_popouts`]).
    ///
    /// **A Rex pane** exits the same way (its relay is the child), and additionally has its Rex
    /// session ended when the program in it finished (`rex::spawn::end_if_exited`, on a worker),
    /// so the next open makes a fresh one. A relay that left for any other reason leaves the
    /// session to be reattached. An agent's relay exiting closes that agent alone
    /// (`Registry::close_agent`): the tab stays live while another of its agents is.
    pub fn handle_child_exited(app: &AppHandle, surface_id: usize) {
        Self::child_exited(app, surface_id);
        close_spent_popouts(app);
    }

    /// [`handle_child_exited`](Self::handle_child_exited)'s work in the window holding the surface.
    fn child_exited(app: &AppHandle, surface_id: usize) {
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
            let Some(key) = ws.registry.surface_key(surface_id) else {
                return;
            };
            let Some((tab_id, which, agent)) = ws.registry.locate_key(&key) else {
                return;
            };
            // A Rex pane's relay exited: its session (an agent's: its window; a column block's:
            // its block) ends too if its program did.
            let rex_key = ws.registry.rex_key_of_surface(surface_id);
            if let Some(key) = &rex_key {
                let owner = agent.clone().map(|r| (tab_id.clone(), r));
                crate::rex::spawn::end_if_exited(app, key.clone(), owner);
            }
            let new_active = match (which, agent) {
                (crate::registry::PaneIdx::Primary, Some(record)) => {
                    match ws.registry.close_agent(&tab_id, &record) {
                        Some(crate::registry::AgentClosed::PaneCold(next)) => next,
                        // Another agent of the pane is live and on screen now: the tab stays.
                        _ => {
                            lock.record_session(app, &label);
                            let dto = lock.init_dto(&label);
                            drop(lock);
                            if let Some(dto) = dto {
                                let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
                            }
                            return;
                        }
                    }
                }
                (crate::registry::PaneIdx::Block(_), _) if rex_key.is_some() => {
                    // A Rex column's block: the column follows what the hub finds of its block
                    // (`end_if_exited` above) — the session's next view drops it, or the
                    // session's end drops the column. Until then its slot sits cold.
                    ws.registry.release_block(&key);
                    lock.record_session(app, &label);
                    let dto = lock.init_dto(&label);
                    drop(lock);
                    if let Some(dto) = dto {
                        let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
                    }
                    return;
                }
                (crate::registry::PaneIdx::Block(i), _) => {
                    ws.registry.close_block(&tab_id, i);
                    drop(lock);
                    announce_pane_closed(app, &label, &tab_id);
                    return;
                }
                (crate::registry::PaneIdx::Primary, None) => ws.registry.unload(&tab_id),
            };
            lock.record_session(app, &label);
            drop(lock);
            // Per-window event: `emit_to` leaks to sibling webviews, so stamp the label and let the
            // chrome filter (see CLAUDE.md).
            let _ = app.emit_to(
                label.as_str(),
                "warden:tab-exited",
                serde_json::json!({ "label": label, "id": tab_id, "newActive": new_active }),
            );
        }
    }

    /// Record that the surface `surface_id` became first responder — the user clicked into it, or
    /// `activate` focused it — as the focused pane of its tab (`Registry::focus_pane`), and tell
    /// that window's chrome (`warden:pane-focused`) so the focused-pane marker follows. This is
    /// the one route that catches a click on LIVE terminal content: the native `NSView` sits above
    /// the webview and consumes the click, so the chrome's own per-pane `mousedown` (which drives
    /// `focus_pane` for a cold pane's backstop) never fires for it. Both routes land on the same
    /// registry record.
    pub fn handle_surface_focused(app: &AppHandle, surface_id: usize) {
        let state = app.state::<ManagerState>();
        let mut lock = state.lock();
        let Some((label, _, _)) = lock.locate_surface(surface_id) else {
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

    /// A right-click asked for warden's menu at `(x, y)` on a surface. The page holds the one
    /// thing the menu can't learn here — the split's live ratio, a divider drag included — so it
    /// gets `warden:pane-menu` and answers with `pane_menu`, which pops the menu.
    pub fn handle_context_menu(app: &AppHandle, surface_id: usize, x: f64, y: f64) {
        let state = app.state::<ManagerState>();
        let lock = state.lock();
        let Some((label, _, _)) = lock.locate_surface(surface_id) else {
            return;
        };
        let Some((tab_id, which)) = lock
            .windows
            .get(&label)
            .and_then(|ws| ws.registry.locate_surface(surface_id))
        else {
            return;
        };
        drop(lock);
        let _ = app.emit_to(
            label.as_str(),
            "warden:pane-menu",
            serde_json::json!({ "label": label, "id": tab_id, "pane": which.index(), "x": x, "y": y }),
        );
    }

    /// The pointer moved onto a surface, so it has just left that window's sidebar. Tell the
    /// chrome, which cannot see it for itself: the surface composites above the webview and takes
    /// the pointer at the boundary, so a fast flick out of the sidebar delivers no further event to
    /// the page and its CSS `:hover` freezes (chrome-core's de-emphasised main list stayed lit).
    pub fn handle_pointer_entered(app: &AppHandle, surface_id: usize) {
        let state = app.state::<ManagerState>();
        let lock = state.lock();
        let Some((label, _, _)) = lock.locate_surface(surface_id) else {
            return;
        };
        drop(lock);
        let _ = app.emit_to(
            label.as_str(),
            "warden:pointer-away",
            serde_json::json!({ "label": label }),
        );
    }

    /// The live surface pane `pane` of tab `tab` shows in window `label`.
    pub fn pane_surface(
        &self,
        label: &str,
        tab: &str,
        pane: crate::registry::PaneIdx,
    ) -> Option<&GhosttySurface> {
        self.windows.get(label)?.registry.shown_surface(tab, pane)
    }

    /// Route a surface signal: find the (window-label, tab-id) owning surface `surface_id`, and
    /// whether its window puts it on screen (`Registry::shows_surface`) — visible once that window
    /// is focused too, which the caller asks Tauri.
    pub fn locate_surface(&self, surface_id: usize) -> Option<(String, String, bool)> {
        self.windows.iter().find_map(|(label, ws)| {
            let tab = ws.registry.tab_of_surface(surface_id)?;
            let shown = ws.registry.shows_surface(surface_id);
            Some((label.clone(), tab.to_string(), shown))
        })
    }

    /// The size of the live Rex pane surface `surface_id` shows; `None` for a plain pane.
    fn rex_size_of_surface(&self, surface_id: usize) -> Option<crate::rex::size::PaneSize> {
        self.windows
            .values()
            .find_map(|ws| ws.registry.rex_size_of_surface(surface_id))
    }

    /// The user turned to a Rex pane ([`SurfaceSignal::UserInput`]): claim its block for
    /// warden's size, on the size worker. A press while warden already owns the block at this
    /// size ends in the hub without a request.
    ///
    /// [`SurfaceSignal::UserInput`]: crate::surface::SurfaceSignal::UserInput
    pub fn handle_user_input(app: &AppHandle, surface_id: usize) {
        let size = app
            .state::<ManagerState>()
            .lock()
            .rex_size_of_surface(surface_id);
        if let Some(size) = size {
            crate::rex::size::request(app, crate::rex::size::SizeAsk::Claim, size);
        }
    }

    /// Return pop-out `label`'s tab to its origin as the pop-out window closes
    /// (`shell_core::detach::wire_return`'s `on_close`, via `main.rs::redock`). Runs on the main
    /// thread under the `ManagerState` lock. Returns what the caller announces once the lock is
    /// released: the window the tab went home to (menu rebuild + refresh) and, when it came home
    /// with nothing up or was dropped, the tab the selection leaned to (`Redocked::dropped`);
    /// `None` if `label` is no pop-out (already returned: a double close).
    ///
    /// Home is [`return_target`]'s answer. A closed origin the caller has already reopened
    /// (`ReturnTarget::Reopen`), so `build_window` stood the tab's placeholder there; with no
    /// window carrying the origin's title (it left the config) the tab has no home and its
    /// surfaces close — the one place a live terminal is intentionally ended. So does a tab whose
    /// placeholder is gone, the origin rebuilt without it: [`Registry::adopt_tab`] refuses it.
    ///
    /// Once home, the tab is reconciled against `last_good` (`reconcile_returned_tab`), fed the
    /// spec the moved tab still runs: every config edit or rescan that touched it while it was
    /// out reached only its placeholder, which `adopt_tab` overwrites, so this is where a deleted
    /// tab goes, a changed `dir`/`cmd` respawns, and a relabel lands.
    pub fn redock(&mut self, app: &AppHandle, label: &str) -> Option<Redocked> {
        let home = match self.origin_home(label)? {
            ReturnTarget::Open(l) => l,
            ReturnTarget::Reopen(_) => self.windows.get(label)?.origin.as_ref()?.label.clone(),
        };
        let ws = self.windows.remove(label)?;
        self.sync_popouts(app);
        let origin = ws.origin?;
        let mut popped = ws.registry;
        let tab_id = origin.tab_id;
        let Some(mut moved) = popped.take_tab(&tab_id) else {
            // Nothing came back to fill the placeholder: its row goes cold rather than reading
            // popped out with no pop-out.
            if let Some(ws) = self.windows.get_mut(&home) {
                ws.registry.clear_placeholder(&tab_id);
            }
            return Some(Redocked {
                origin_label: home,
                dropped: None,
            });
        };
        let Some(ws) = self
            .windows
            .get_mut(&home)
            .filter(|ws| ws.origin.is_none() && same_title(&ws.title, &origin.title))
        else {
            log::info!("tab {tab_id:?}: no window titled {:?}, ended", origin.title);
            moved.close();
            return Some(Redocked {
                origin_label: home,
                dropped: None,
            });
        };
        let title = ws.title.clone();
        // The tab as `last_good` has it now, plus that window's tab order (a respawn appends).
        let config = self.spec_for(&home, &title);
        let order: Vec<String> = config
            .iter()
            .flat_map(|s| s.tabs.iter().map(|t| t.spec.id.clone()))
            .collect();
        let plan = config.and_then(|s| s.tabs.into_iter().find(|t| t.spec.id == tab_id));
        let spawned = moved.spec().clone();
        let ws = self.windows.get_mut(&home)?;
        let up = moved.is_up();
        let landed = match ws.registry.adopt_tab(moved) {
            Ok(()) => up,
            // No placeholder to land in, or a surface that won't move: the tab ends, and comes
            // home cold where its placeholder still stands, so its row stops reading popped out.
            Err(mut back) => {
                log::error!("tab {tab_id:?}: couldn't go home to {home:?}, ended");
                back.close();
                let _ = ws.registry.adopt_tab(back);
                false
            }
        };
        let mut dropped =
            reconcile_returned_tab(&mut ws.registry, &tab_id, &spawned, plan.as_ref())
                .map(|new_active| (tab_id.clone(), new_active));
        ws.registry.reorder(&order);
        if dropped.is_none() {
            match ws.registry.active_tab() {
                // Back with nothing up while it is the selection: lean, as an unload does.
                Some(a) if a == tab_id && !landed => {
                    let next = ws.registry.lean_away(&tab_id);
                    dropped = Some((tab_id.clone(), next));
                }
                // Re-assert the origin's CURRENT selection — never force the returned tab: the
                // chrome owns selection (warden passes no `active`), and showing the returned
                // tab under another row's highlight leaves the shown surface and the sidebar
                // apart. With nothing selected, the returned tab is the one to show.
                Some(a) => {
                    let a = a.to_owned();
                    let _ = ws.registry.activate(&a);
                }
                None if landed => {
                    let _ = ws.registry.activate(&tab_id);
                }
                None => {}
            }
        }
        self.record_session(app, &home);
        Some(Redocked {
            origin_label: home,
            dropped,
        })
    }

    /// Save `label`'s loaded + active tabs for `remember_tabs` — or forget them when the window
    /// has it off. Call after anything that changes a window's registry. Skipped while quitting:
    /// ⌘Q's teardown (and any child exits it provokes) must not overwrite the set being
    /// remembered. A closed window records nothing either, so its last set survives for a reopen.
    pub fn record_session(&mut self, app: &AppHandle, label: &str) {
        if !is_quitting(app) {
            self.record(label);
        }
    }

    /// [`record_session`](Self::record_session) outside a quit. A pop-out remembers nothing: its
    /// tab is remembered by its origin, whose placeholder counts as loaded.
    fn record(&mut self, label: &str) {
        let Some(ws) = self.windows.get(label).filter(|ws| ws.origin.is_none()) else {
            return;
        };
        let remember = self
            .last_good
            .windows
            .iter()
            .any(|w| w.title == ws.title && w.remember_tabs);
        let record = remember.then(|| crate::session::WindowRecord {
            loaded: ws.registry.loaded_ids(),
            active: ws.registry.active_tab().map(str::to_string),
        });
        self.session.set(&ws.session_key, record);
    }

    /// `record_session` for every open window — after a hot-reload or rescan, once `last_good`
    /// holds the new config (so a `remember_tabs` flip takes effect here too).
    pub fn record_all_sessions(&mut self, app: &AppHandle) {
        let labels: Vec<String> = self.windows.keys().cloned().collect();
        for label in labels {
            self.record_session(app, &label);
        }
    }

    /// The window `⌘⇧T` would reopen right now, given the `WindowSet`'s `entries` — the one
    /// computation behind both the action and its menu item's enabled state.
    pub fn reopen_target(&self, entries: &[shell_core::menu::WindowEntry]) -> Option<String> {
        crate::plan::next_reopen_target(&self.last_closed, entries)
    }

    /// Bring the open windows' tabs in line with a reloaded config, and make `new_config` (the
    /// *new* effective config, roots already expanded) the baseline `last_good`. Opening, closing
    /// and recolouring windows is the `WindowSet`'s (`WindowSet::apply`, run by the caller after
    /// this with the lock released); `label_of` is its title → label lookup. Each kept window's
    /// registry is mutated in place and a fresh snapshot pushed (`init_dto`, read after the
    /// baseline advances, so a flipped global reaches the chrome) so the chrome rebuilds its
    /// sidebar. `new_config`'s roots are what `tab_ops` derives tree metadata from for added tabs.
    pub fn apply(
        &mut self,
        app: &AppHandle,
        recon: &Reconciliation,
        new_config: Config,
        label_of: impl Fn(&str) -> Option<String>,
    ) {
        let ops = tab_ops(recon, &new_config, label_of);
        // A popped-out tab's live edits — a relabel, a split appearing or going — reach the
        // pop-out holding it now: its placeholder in the origin is skipped below, and overwritten
        // on return. A respawn waits for the return (`reconcile_returned_tab`).
        let forward: Vec<(String, Forward)> = ops
            .iter()
            .flat_map(|op| {
                let meta = op
                    .set_meta
                    .iter()
                    .map(|m| (&m.id, Forward::Meta(m.clone())));
                let split = op
                    .split_tabs
                    .iter()
                    .map(|t| (&t.spec.id, Forward::Split(t.clone())));
                let unsplit = op
                    .unsplit_tabs
                    .iter()
                    .map(|id| (id, Forward::Unsplit(id.clone())));
                meta.chain(split)
                    .chain(unsplit)
                    .filter_map(|(id, f)| Some((self.popout_of(&op.label, id)?.to_owned(), f)))
                    .collect::<Vec<_>>()
            })
            .collect();
        // Windows whose tree sections changed: no tab op carries that, only the refresh's
        // `InitDto.trees` does, so an empty root added or removed must still refresh.
        let trees_changed: HashSet<String> = new_config
            .windows
            .iter()
            .filter(|nw| {
                self.last_good
                    .windows
                    .iter()
                    .find(|ow| ow.title == nw.title)
                    .is_some_and(|ow| ow.tree_names() != nw.tree_names())
            })
            .map(|nw| nw.title.clone())
            .collect();
        self.last_good = new_config;
        let mut refreshed: Vec<String> = Vec::new();
        let mut exited: Vec<(String, String, Option<String>)> = Vec::new();
        for op in ops {
            let crate::plan::TabOp {
                label,
                add_tabs,
                remove_tabs,
                order,
                set_meta,
                respawn_tabs,
                split_tabs,
                unsplit_tabs,
                spawn_tabs,
                open_tabs_section,
                background_tabs_section,
            } = op;
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
                if !trees_changed.contains(&ws.title)
                    && open_tabs_section.is_none()
                    && background_tabs_section.is_none()
                    && add_tabs.is_empty()
                    && remove_tabs.is_empty()
                    && order == current_order
                    && set_meta.is_empty()
                    && respawn_tabs.is_empty()
                    && split_tabs.is_empty()
                    && unsplit_tabs.is_empty()
                    && spawn_tabs.is_empty()
                {
                    continue;
                }
                if let Some(o) = open_tabs_section {
                    ws.open_tabs_section = o;
                }
                if let Some(o) = background_tabs_section {
                    ws.background_tabs_section = o;
                }
                // Removing the visible tab: unload it first so the selection leans to a live
                // neighbour, and tell the chrome before the refresh drops its row — otherwise
                // chrome-core falls back to the first tab and spawns it if cold (`redock` does
                // the same for a tab that left the config while out).
                if let Some(active) = was_active.as_deref() {
                    if remove_tabs.iter().any(|id| id == active)
                        && !ws.registry.is_placeholder(active)
                    {
                        let new_active = ws.registry.unload(active);
                        exited.push((label.clone(), active.to_string(), new_active));
                    }
                }
                // Add/remove/respawn tabs, skipping placeholders — see `apply_tab_reconcile`.
                apply_tab_reconcile(&mut ws.registry, &remove_tabs, &add_tabs, &respawn_tabs);
                // Apply in-place metadata (group/probe/kill + recomputed
                // tree/tree_path) without respawning; the warden:refresh below
                // pushes fresh DTOs (has_probe/has_kill recomputed) and the
                // post-reload bump_all fast-bursts every window. Tree-ness CAN
                // flip for a *kept* tab: a curated tab whose normalized dir key
                // equals a `[[window.root]]` discovery's path key shadows it, so
                // reconcile sees the same key move between the root section
                // (tree) and the curated group (not) — a set_meta, not add/
                // remove. `tab_ops` recomputes `tree`/`tree_path` from the
                // new group; applying them here keeps the shadowed row from
                // rendering as a stale tree row (or vice-versa).
                for m in &set_meta {
                    ws.registry
                        .set_meta(&m.id, &m.meta, m.tree, m.tree_path.clone());
                }
                apply_split_changes(&mut ws.registry, &split_tabs, &unsplit_tabs);
                // load_on_open turned on: bring a cold tab up now, as a launch
                // would. A live tab is untouched, and a popped-out one is live.
                for id in &spawn_tabs {
                    if ws.registry.is_placeholder(id) {
                        continue;
                    }
                    if let Err(e) = ws.registry.ensure_spawned_by_id(id) {
                        log::error!("spawn on load_on_open failed for {id:?}: {e}");
                    }
                }
                ws.registry.reorder(&order);
                // If the on-screen tab was respawned, it went cold — re-activate it
                // so its new surface spawns and shows in place (no blank placeholder).
                // Runs BEFORE tab_dtos below so the re-activated surface's spawned
                // state is reflected in the refresh snapshot.
                if let Some(active) = &was_active {
                    if respawn_tabs.iter().any(|tp| &tp.spec.id == active) {
                        if let Err(e) = ws.registry.activate(active) {
                            log::error!("reactivate after respawn failed: {e}");
                        }
                    }
                }
                refreshed.push(label);
            }
        }
        for (label, f) in forward {
            let Some(ws) = self.windows.get_mut(&label) else {
                continue;
            };
            match f {
                // The pop-out is titled by its tab: its banner (the refresh below carries
                // `InitDto.title`) and its native window follow a relabel.
                Forward::Meta(m) => {
                    if ws.title != m.meta.title {
                        ws.title = m.meta.title.clone();
                        if let Some(w) = app.get_window(&label) {
                            let _ = w.set_title(&ws.title);
                        }
                    }
                    ws.registry.set_meta(&m.id, &m.meta, m.tree, m.tree_path)
                }
                Forward::Split(tp) => apply_split_changes(&mut ws.registry, &[tp], &[]),
                Forward::Unsplit(id) => apply_split_changes(&mut ws.registry, &[], &[id]),
            }
            if !refreshed.contains(&label) {
                refreshed.push(label);
            }
        }
        // Push each updated window's new snapshot so the chrome rebuilds its sidebar.
        // `emit_to(label, …)`, never `Emitter::emit` (a global broadcast in Tauri 2.11.3,
        // whatever the receiver): emitting on the window would fire every sibling's listener.
        for (label, id, new_active) in exited {
            let _ = app.emit_to(
                label.as_str(),
                "warden:tab-exited",
                serde_json::json!({ "label": label, "id": id, "newActive": new_active }),
            );
        }
        for label in refreshed {
            if let Some(dto) = self.init_dto(&label) {
                let _ = app.emit_to(label.as_str(), "warden:refresh", dto);
            }
        }
    }
}

/// One of a popped-out tab's hot-reload edits, for the pop-out holding it (`WindowManager::apply`).
enum Forward {
    Meta(crate::plan::MetaPlan),
    Split(TabPlan),
    Unsplit(String),
}

/// Bring a tab just returned from a pop-out (`redock`, after `adopt_tab`) in line with the config
/// it comes home to — the reconcile `apply_tab_reconcile` skipped while it was out. `plan` is
/// the tab in `last_good`'s origin window (`None`: it left the config, or vanished from a
/// rescanned root); `spawned` is the spec the moved tab's surfaces run, compared rather than the
/// origin placeholder's, which a hot-reload or a rebuilt origin has moved on.
///
/// - Gone → unloaded, then removed: the unload is what leans the selection to a live neighbour
///   when this was the visible tab, exactly as unloading it by hand does. Returns
///   `Some(new_active)` (`Registry::unload`'s answer) so the caller can tell the chrome.
/// - [`warden_config::TerminalSpec`] changed → [`SpecChange`] decides, exactly as for a docked
///   tab: a respawn goes through `apply_tab_reconcile` (the entry moves to the end; the caller
///   reorders), a split appearing or going through `apply_split_changes`.
/// - Otherwise, and after a split change → `set_meta` from the plan (title/group/probe/kill/
///   suspend/split layout/tree).
///
/// `None` whenever the tab stays.
fn reconcile_returned_tab(
    registry: &mut Registry,
    id: &str,
    spawned: &TabSpec,
    plan: Option<&TabPlan>,
) -> Option<Option<String>> {
    let Some(tp) = plan else {
        let new_active = registry.unload(id);
        registry.remove(id);
        return Some(new_active);
    };
    fn terminal(s: &TabSpec) -> warden_config::TerminalSpec<'_> {
        warden_config::TerminalSpec::new(
            &s.dir,
            &s.shell,
            s.startup.as_deref(),
            s.split.as_ref(),
            s.server.as_deref(),
        )
    }
    let n = &tp.spec;
    match terminal(spawned).change_to(&terminal(n)) {
        SpecChange::Respawn => {
            apply_tab_reconcile(registry, &[], &[], std::slice::from_ref(tp));
            return None;
        }
        SpecChange::Split => apply_split_changes(registry, std::slice::from_ref(tp), &[]),
        SpecChange::Unsplit => apply_split_changes(registry, &[], &[id.to_string()]),
        SpecChange::Same => {}
    }
    let meta = warden_config::TabMeta {
        group: n.group.clone(),
        probe: n.probe.clone(),
        kill: n.kill.clone(),
        suspend: n.suspend.clone(),
        resume: n.resume.clone(),
        title: n.title.clone(),
        split: n.split.clone(),
        colour: n.colour,
    };
    registry.set_meta(id, &meta, n.tree, n.tree_path.clone());
    None
}

/// Open the second pane of each tab in `split_tabs` beside its running primary, and close it on
/// each tab in `unsplit_tabs` — skipping a placeholder, as `apply_tab_reconcile` does: the
/// pop-out holding the tab takes the change (`WindowManager::apply`'s forward).
fn apply_split_changes(registry: &mut Registry, split_tabs: &[TabPlan], unsplit_tabs: &[String]) {
    for tp in split_tabs {
        if registry.is_placeholder(&tp.spec.id) {
            continue;
        }
        if let Some(split) = &tp.spec.split {
            registry.split_from_config(&tp.spec.id, split);
        }
    }
    for id in unsplit_tabs {
        if registry.is_placeholder(id) {
            continue;
        }
        registry.unsplit(id);
    }
}

/// Apply a hot-reload's tab-set reconcile ops (from the config diff) to one
/// window's registry — **skipping any placeholder**, a tab moved out to a pop-out.
///
/// `tab_ops` is derived purely from the config diff and knows nothing about
/// runtime pop-out state, so a config edit to a popped-out tab would otherwise
/// clobber its placeholder: `remove`/`respawn`'s `remove` drops the slot the tab
/// must return to (→ `adopt_tab` refuses it and its PTYs end), and
/// `add`/`respawn`'s `add` eagerly spawns a duplicate surface for a tab already
/// live elsewhere. Guarding each op on `is_placeholder` leaves `redock` the sole
/// owner of a popped-out tab's lifecycle: once the tab is home it reconciles it
/// against `last_good` (`reconcile_returned_tab`), which by then carries every edit
/// made while it was out.
///
/// The skip keys on `Tab::key` (`id`-else-normalized-`dir`) exactly as the
/// registry keys every entry — `remove_tabs` carries those keys directly, and a
/// `TabPlan`'s `spec.id` *is* that key. Any other tab reconciles unchanged.
fn apply_tab_reconcile(
    registry: &mut Registry,
    remove_tabs: &[String],
    add_tabs: &[TabPlan],
    respawn_tabs: &[TabPlan],
) {
    for id in remove_tabs {
        // A placeholder is redock's to manage — dropping it here leaves the tab no slot
        // to return to.
        if registry.is_placeholder(id) {
            continue;
        }
        registry.remove(id);
    }
    for tp in add_tabs {
        // An add for a popped-out key would spawn a duplicate PTY for a tab the
        // placeholder already represents. (An add normally only fires for a
        // genuinely new key; this guard is defensive.)
        if registry.is_placeholder(&tp.spec.id) {
            continue;
        }
        // A failed eager spawn on hot-reload leaves the tab cold (it retries on
        // focus, which surfaces the error via the banner then) — log it, never panic.
        if let Err(e) = registry.add(&tp.spec, tp.load_on_open) {
            log::error!("surface spawn failed for tab {:?}: {e}", tp.spec.title);
        }
    }
    // Respawn kept tabs whose terminal spec changed: tear down and rebuild by the
    // same id (identity is stable). A cold tab just gets a fresh spec and lazy-spawns
    // on next focus; a load_on_open tab respawns eagerly. The previously-active tab is
    // re-activated by the caller so a visible respawn shows its new surface immediately.
    for tp in respawn_tabs {
        // A popped-out tab is out on loan: its surfaces live in the pop-out's
        // registry, so rebuilding the placeholder here would leave it no slot. Skip —
        // `redock` reconciles it against the config once it's home.
        if registry.is_placeholder(&tp.spec.id) {
            continue;
        }
        registry.remove(&tp.spec.id);
        if let Err(e) = registry.add(&tp.spec, tp.load_on_open) {
            log::error!("surface respawn failed for tab {:?}: {e}", tp.spec.title);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::TabSpec;
    use std::sync::atomic::Ordering;

    /// A `warden://` `tab` value matches an id before a title, and the first of repeated titles.
    #[test]
    fn tab_named_prefers_id_then_first_title() {
        let tabs = [
            ("/a/console", "console"),
            ("console", "other"),
            ("/b/console", "console"),
        ];
        assert_eq!(tab_named(&tabs, "console"), Some("console"));
        assert_eq!(tab_named(&tabs[..1], "console"), Some("/a/console"));
        assert_eq!(
            tab_named(&[tabs[0], tabs[2]], "console"),
            Some("/a/console")
        );
        assert_eq!(tab_named(&tabs, "missing"), None);
    }

    fn config(src: &str) -> Config {
        warden_config::resolve::resolve(warden_config::raw::parse(src).unwrap())
            .unwrap()
            .0
    }

    fn record() -> crate::session::WindowRecord {
        crate::session::WindowRecord {
            loaded: vec!["/tmp".into()],
            active: None,
        }
    }

    /// shell-core reports a closed window's rename under its pre-rename label ("a-b"), which the
    /// set has since given to the other window whose title slugs to it ("a-b", was "a-b-2").
    #[test]
    fn retitle_of_a_closed_window_follows_its_new_title_and_leaves_the_slug_twin_alone() {
        let mut m = WindowManager::new();
        let c = config("[[window]]\ntitle = \"a b\"\n[[window]]\ntitle = \"a-b\"\n");
        m.adopt(c.clone(), c);
        m.last_closed = vec!["a b".into(), "a-b".into()];
        m.session.set("a-b", Some(record()));
        let red = Colour::parse("#ff0000").unwrap();
        m.retitle("a-b", " a b ", "x", red);

        let titles = |c: &Config| {
            c.windows
                .iter()
                .map(|w| w.title.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(&m.last_good), ["x", "a-b"]);
        assert_eq!(titles(&m.raw_config), ["x", "a-b"]);
        assert_eq!(m.last_good.windows[0].colour, red);
        assert_ne!(
            m.last_good.windows[1].colour, red,
            "the slug twin is untouched"
        );
        // ⌘⇧T's stack and (through the same loop) pop-outs' origin titles follow the rename.
        assert_eq!(m.last_closed, ["x", "a-b"]);
        // The remembered tabs move from the old launch label to the new one.
        assert!(m.session.get("a-b").is_none());
        assert_eq!(m.session.get("x"), Some(&record()));
    }

    /// A Delete Window whose reload hasn't landed yet: `last_good` still lists the deleted window
    /// ahead of the renamed one, so the set's positions no longer line up with it.
    #[test]
    fn retitle_finds_the_window_by_its_old_title_while_a_delete_is_in_flight() {
        let mut m = WindowManager::new();
        let c = config("[[window]]\ntitle = \"gone\"\n[[window]]\ntitle = \"work\"\n");
        m.adopt(c.clone(), c);
        let red = Colour::parse("#ff0000").unwrap();
        m.retitle("work", "work", "job", red);
        let titles: Vec<_> = m
            .last_good
            .windows
            .iter()
            .map(|w| w.title.as_str())
            .collect();
        assert_eq!(titles, ["gone", "job"]);
    }

    #[test]
    fn probe_interval_defaults_to_5_and_is_settable() {
        let m = WindowManager::new();
        assert_eq!(m.probe_interval.load(Ordering::Relaxed), 5);
        m.set_probe_interval(0);
        assert_eq!(m.probe_interval.load(Ordering::Relaxed), 0);
    }

    fn tab_spec(id: &str, dir: &str) -> TabSpec {
        TabSpec {
            colour: None,
            id: id.into(),
            title: id.into(),
            dir: std::path::PathBuf::from(dir),
            shell: "sh".into(),
            startup: None,
            group: None,
            probe: None,
            kill: None,
            suspend: None,
            tree: false,
            tree_path: Vec::new(),
            split: None,
            server: None,
            resume: None,
        }
    }

    // `apply` itself needs a live `AppHandle` (Tauri) and a real `GhosttySurface`
    // (AppKit) — both unconstructable in a unit-test process — so the placeholder-aware
    // guard is exercised at its narrowest real layer: `apply_tab_reconcile`, the free
    // function `apply` delegates each `TabOp`'s add/remove/respawn to.
    // `stand_placeholder` reaches a placeholder without a live surface.
    #[test]
    fn reconcile_leaves_a_popped_out_tabs_placeholder_alone() {
        // A hot-reload whose config diff removes/respawns a currently popped-out tab
        // must NOT touch its placeholder: the live surfaces live in the pop-out's
        // registry and `redock` is their sole owner. Clobbering the placeholder either
        // strands them (remove → the return finds no slot and ends the PTYs, silent data
        // loss) or spawns a duplicate (respawn's/ add's `add`).
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/old"), false);
        let _ = r.add(&tab_spec("t1", "/tmp/b"), false);
        r.stand_placeholder("t0");

        // Respawn variant: config changed t0's dir (id stable) → respawn_tabs=[t0]. A
        // sibling t1 here is genuinely removed to prove the normal path is intact.
        let respawn = vec![TabPlan {
            spec: tab_spec("t0", "/tmp/new"),
            load_on_open: false,
        }];
        apply_tab_reconcile(&mut r, &["t1".to_string()], &[], &respawn);
        assert!(
            r.is_placeholder("t0"),
            "the placeholder must survive a respawn op"
        );
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec!["t0".to_string()],
            "t0 kept (a placeholder), t1 removed via the normal path, no duplicate entry"
        );
        assert!(
            !r.is_spawned("t0"),
            "no duplicate surface spawned for the popped-out tab"
        );

        // Remove variant: config deleted t0 while it's out → remove_tabs=[t0].
        apply_tab_reconcile(&mut r, &["t0".to_string()], &[], &[]);
        assert!(
            r.is_placeholder("t0"),
            "the placeholder must survive a remove op (the tab returns on redock)"
        );
        assert_eq!(r.tab_dtos().len(), 1, "still present, not dropped");

        // Add variant (defensive): an add for a popped-out key must not append
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
        assert!(r.is_placeholder("t0"));
        assert_eq!(
            r.tab_dtos().len(),
            1,
            "add for a popped-out key must not create a duplicate entry"
        );
    }

    // `redock` runs this after `adopt_tab`, so the entry is the moved tab; a `Cold` one stands in
    // for it here (no AppKit) — what matters is that it is no placeholder, which the reconcile
    // guards skip.
    fn plan(spec: TabSpec) -> TabPlan {
        TabPlan {
            spec,
            load_on_open: false,
        }
    }

    #[test]
    fn a_curated_tab_with_an_id_shadows_a_same_dir_discovered_project() {
        // An `id` makes the curated key differ from the dir key the scanner gives the same
        // project; the shadow still holds, matched by dir.
        use warden_config::{Colour, Density, Root, Tab, TabDigitKeys, Window};
        let base = std::env::temp_dir().join(format!("warden-shadow-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let proj = base.join("proj");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        let curated = Tab {
            colour: None,
            id: Some("mine".into()),
            key: "mine".into(),
            title: "curated".into(),
            dir: proj.clone(),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: None,
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
        };
        let root = Root {
            name: "R".into(),
            dir: base.clone(),
            depth: 6,
            shell: "sh".into(),
            startup: None,
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
            overrides: Vec::new(),
        };
        let cfg = Config {
            windows: vec![Window {
                title: "w".into(),
                colour: Colour { r: 0, g: 0, b: 0 },
                width: 1500,
                height: 1000,
                open_on_start: true,
                open_tabs_section: false,
                agents_tab: false,
                background_tabs_section: false,
                remember_tabs: false,
                tabs: vec![curated],
                roots: vec![root],
            }],
            format_on_save: false,
            tab_digit_keys: TabDigitKeys::default(),
            probe_interval: 5,
            density: Density::default(),
            sidebar_drag: true,
            auto_update: true,
            notify_debug: false,
            trusted_link_schemes: Vec::new(),
        };
        let eff = effective_config(&cfg, None);
        let keys: Vec<_> = eff.windows[0].tabs.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["mine"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_returned_tab_gone_from_config_is_dropped() {
        // Deleted from the config (or vanished from a rescanned root) while popped out: the
        // reconcile skipped its placeholder, so the return is where it ends — no orphan row,
        // and a later re-add of the key can't duplicate it.
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/a"), false);
        let _ = r.add(&tab_spec("t1", "/tmp/b"), false);
        assert_eq!(
            reconcile_returned_tab(&mut r, "t0", &tab_spec("t0", "/tmp/a"), None),
            Some(None),
            "dropped; nothing live to lean to"
        );
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t1".to_string()]);
    }

    #[test]
    fn a_returned_tab_whose_terminal_spec_changed_is_respawned_with_the_new_spec() {
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/old"), false);
        let mut new = tab_spec("t0", "/tmp/new");
        new.startup = Some("amux".into());
        reconcile_returned_tab(&mut r, "t0", &tab_spec("t0", "/tmp/old"), Some(&plan(new)));
        let s = r.spec_of("t0").expect("still present");
        assert_eq!(s.dir, std::path::PathBuf::from("/tmp/new"));
        assert_eq!(s.startup.as_deref(), Some("amux"));
        assert_eq!(r.tab_dtos().len(), 1, "respawned in place, not duplicated");
    }

    #[test]
    fn a_returned_tab_compares_against_the_spec_its_surface_runs() {
        // An origin rebuilt while the tab was out already carries the NEW spec, so comparing
        // the registry against the config finds nothing; the spec the surface was spawned with
        // is what decides.
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/new"), false);
        let _ = r.add(&tab_spec("t1", "/tmp/b"), false);
        reconcile_returned_tab(
            &mut r,
            "t0",
            &tab_spec("t0", "/tmp/old"),
            Some(&plan(tab_spec("t0", "/tmp/new"))),
        );
        // Respawn is remove + add, which appends the entry (the caller's `reorder` restores
        // config order) — so the move is the observable proof the respawn ran.
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t1".to_string(), "t0".to_string()]);
    }

    #[test]
    fn a_returned_tab_whose_split_appeared_or_went_keeps_its_primary() {
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/a"), false);
        let _ = r.add(&tab_spec("t1", "/tmp/b"), false);
        let mut split = tab_spec("t0", "/tmp/a");
        split.split = Some(warden_config::Split {
            side: warden_config::SplitSide::Right,
            size: 0.5,
            panes: vec![warden_config::StackPane {
                size: 1.0,
                startup: None,
            }],
        });
        reconcile_returned_tab(
            &mut r,
            "t0",
            &tab_spec("t0", "/tmp/a"),
            Some(&plan(split.clone())),
        );
        assert!(r.is_split("t0"));
        reconcile_returned_tab(&mut r, "t0", &split, Some(&plan(tab_spec("t0", "/tmp/a"))));
        assert!(!r.is_split("t0"));
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t0".to_string(), "t1".to_string()], "no respawn");
    }

    /// A popped-out tab's split is `redock`'s to bring in line once home: a hot-reload's split
    /// change leaves its placeholder exactly as it was.
    #[test]
    fn split_changes_skip_a_popped_out_tab() {
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let split = warden_config::Split {
            side: warden_config::SplitSide::Right,
            size: 0.5,
            panes: vec![warden_config::StackPane {
                size: 1.0,
                startup: None,
            }],
        };
        let mut gains = tab_spec("t0", "/tmp/a");
        let _ = r.add(&gains, false);
        let mut loses = tab_spec("t1", "/tmp/b");
        loses.split = Some(split.clone());
        let _ = r.add(&loses, false);
        r.stand_placeholder("t0");
        r.stand_placeholder("t1");
        gains.split = Some(split);
        apply_split_changes(&mut r, &[plan(gains)], &["t1".to_string()]);
        assert!(!r.is_split("t0"));
        assert_eq!(r.split_side("t0"), None);
        assert!(r.split_side("t1").is_some());
        assert!(r.is_placeholder("t0") && r.is_placeholder("t1"));
    }

    #[test]
    fn a_returned_tab_with_only_metadata_changes_is_relabelled_in_place() {
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = r.add(&tab_spec("t0", "/tmp/a"), false);
        let _ = r.add(&tab_spec("t1", "/tmp/b"), false);
        let mut new = tab_spec("t0", "/tmp/a");
        new.title = "renamed".into();
        new.probe = Some("p".into());
        reconcile_returned_tab(&mut r, "t0", &tab_spec("t0", "/tmp/a"), Some(&plan(new)));
        assert_eq!(r.tab_title("t0").as_deref(), Some("renamed"));
        assert!(r.tab_has_probe("t0"));
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t0".to_string(), "t1".to_string()], "no respawn");
    }

    fn origin(label: &str, title: &str) -> PopOrigin {
        PopOrigin {
            label: label.into(),
            title: title.into(),
            tab_id: "/tmp/a".into(),
        }
    }

    /// A configured window's row is the only one; a pop-out's tab is also the origin's
    /// placeholder row, so a badge, a seen or a presence dot reaches both.
    #[test]
    fn rows_for_a_popped_out_tab_include_the_origin_placeholder() {
        assert_eq!(tab_rows("work", "/tmp/a", None), [("work", "/tmp/a")]);
        let from = origin("work", "work");
        assert_eq!(
            tab_rows("shell-detach:k", "/tmp/a", Some(&from)),
            [("shell-detach:k", "/tmp/a"), ("work", "/tmp/a")]
        );
    }

    /// The origin's own label wins over a title lookup, which would take the first window carrying
    /// the title.
    #[test]
    fn return_target_prefers_the_open_origin_label() {
        let open = [("work-2", " work"), ("work", "work")];
        assert_eq!(
            return_target(&origin("work", "work"), &open),
            ReturnTarget::Open("work".into())
        );
    }

    /// Renamed in place (the label stays, `retitle` moved the title), or closed and reopened under
    /// the label its new title slugs to, while another window took the old label.
    #[test]
    fn return_target_finds_a_renamed_origin_by_title() {
        let open = [("work", "other"), ("job", "job")];
        assert_eq!(
            return_target(&origin("work", "job"), &open),
            ReturnTarget::Open("job".into())
        );
    }

    #[test]
    fn return_target_reopens_a_closed_origin_by_title() {
        assert_eq!(
            return_target(&origin("work", "job"), &[("work", "other")]),
            ReturnTarget::Reopen("job".into())
        );
        assert_eq!(
            return_target(&origin("work", "work"), &[]),
            ReturnTarget::Reopen("work".into())
        );
    }

    #[test]
    fn popout_should_close_when_nothing_live() {
        let mut r = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        assert!(popout_should_close(&r), "no tab at all");
        let _ = r.add(&tab_spec("t0", "/tmp/a"), false);
        assert!(popout_should_close(&r), "its one tab is cold");
    }

    fn window_state(registry: Registry, origin: Option<PopOrigin>) -> WindowState {
        WindowState {
            registry,
            title: "w".into(),
            colour: "#000000".into(),
            open_tabs_section: false,
            background_tabs_section: false,
            spawn_error: None,
            session_key: "w".into(),
            chrome_ready: false,
            pending_focus: None,
            origin,
        }
    }

    /// A pop-out's home among the open windows: a configured window under the origin's title
    /// wins over the origin's dead label, and no window under it reopens it by title. Never the
    /// pop-out itself, nor a window that isn't a pop-out.
    #[test]
    fn origin_home_follows_an_origin_rebuilt_under_a_new_label() {
        let mut m = WindowManager::new();
        let empty = || Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let label = shell_core::detach::detached_label("k");
        m.windows.insert(
            label.clone(),
            window_state(empty(), Some(origin("old", "w"))),
        );
        assert_eq!(
            m.origin_home(&label),
            Some(ReturnTarget::Reopen("w".into()))
        );
        m.windows.insert("new".into(), window_state(empty(), None));
        assert_eq!(
            m.origin_home(&label),
            Some(ReturnTarget::Open("new".into()))
        );
        assert_eq!(m.origin_home("new"), None);
    }

    /// A popped-out tab's badge lives under its origin's launch label and tab id, so it outlives the
    /// pop-out, and seeing the tab in its docked window clears it.
    #[test]
    fn a_popped_out_tabs_badge_is_kept_under_its_origin() {
        let mut m = WindowManager::new();
        let c = config("[[window]]\ntitle = \"w\"\n");
        m.adopt(c.clone(), c);
        let key = crate::plan::launch_label(&m.last_good, 0).unwrap();
        let empty = || Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let pop = shell_core::detach::detached_label("k");
        m.windows.insert(
            pop.clone(),
            window_state(empty(), Some(origin("gone", "w"))),
        );
        m.set_unread(&pop, "pop-tab", true);
        assert!(m.unread.has(&key, "/tmp/a"));
        let mut docked = window_state(empty(), None);
        docked.session_key = key.clone();
        m.windows.insert("w".into(), docked);
        m.set_unread("w", "/tmp/a", false);
        assert!(!m.unread.has(&key, "/tmp/a"));
        m.set_unread("unknown", "/tmp/a", true);
        assert!(!m.unread.has("unknown", "/tmp/a"), "no window, no badge");
    }

    /// A pop-out's tab is remembered by its origin; the pop-out itself records nothing, even when a
    /// config window shares its title and remembers tabs.
    #[test]
    fn record_session_skips_popouts() {
        let mut m = WindowManager::new();
        let c = config("[[window]]\ntitle = \"w\"\nremember_tabs = true\n");
        m.adopt(c.clone(), c);
        let mut reg = Registry::new(std::ptr::null_mut(), INITIAL_RECT);
        let _ = reg.add(&tab_spec("/tmp/a", "/tmp/a"), false);
        let label = shell_core::detach::detached_label("k");
        m.windows
            .insert(label.clone(), window_state(reg, Some(origin("w", "w"))));
        m.record(&label);
        assert!(m.session.get("w").is_none());
        assert!(m.session.get(&label).is_none());
        assert!(m.session.get("").is_none());

        m.windows.insert(
            "w".into(),
            window_state(Registry::new(std::ptr::null_mut(), INITIAL_RECT), None),
        );
        m.record("w");
        assert!(
            m.session.get("w").is_some(),
            "a configured window still records"
        );
    }

    fn dto(id: &str) -> TabDto {
        TabDto {
            id: id.to_string(),
            title: id.to_string(),
            warn: None,
            spawned: false,
            group: None,
            has_probe: true,
            has_kill: false,
            has_suspend: false,
            has_cmd: false,
            tree: false,
            tree_path: Vec::new(),
            detached: false,
            secondary_spawned: false,
            split: false,
            split_layout: None,
            column: None,
            agents: false,
            colour: crate::project_colour::project_colour(id, None),
            server: None,
            all_agents: false,
            agents_selected: None,
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
        // Against a stuck-dark dot: probe_now's replay can land while a wide window's sweep is
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
