mod ffi;
mod geometry;
mod plan;
mod scanner;
mod surface;

#[cfg(not(target_os = "macos"))]
compile_error!(
    "warden-app currently targets macOS only (libghostty surface embed). Linux is a later spike."
);

#[cfg(target_os = "macos")]
mod manager;

#[cfg(target_os = "macos")]
mod notify;

#[cfg(target_os = "macos")]
mod probe;

#[cfg(target_os = "macos")]
mod registry;

#[cfg(target_os = "macos")]
use manager::{InitDto, WindowManager};

use geometry::WebRect;

// Menu-item IDs, matched in the Builder's on_menu_event handler. The App/Config/Window submenus,
// Close Tab / Close Window / Pop Out Tab, and the tab-nav block are all the shared spine now
// (`shell_core::menu`) — only warden's own Reopen Last Closed keeps a local id.
// Reopen Last Closed (⌘⇧T) is warden-only — curator and lector have no equivalent, so this stays
// a local item spliced into the spine's Window submenu rather than added to the spine itself
// (YAGNI — add it there only if a sibling app wants it too).
const MENU_WINDOW_REOPEN_LAST: &str = "window_reopen_last";

/// Tab ▸ Split (⌘D) — warden-only, spliced into warden's OWN Tab submenu composition rather than
/// shell-core's shared spine: a split is terminal-specific, and curator/lector have no equivalent
/// (same YAGNI reasoning as `MENU_WINDOW_REOPEN_LAST`). The id doubles as the event name emitted
/// to the focused window — the chrome's own `activeId` decides which tab to split, so the payload
/// carries only the window label, matching Close Tab / Pop Out Tab below.
const MENU_TAB_SPLIT: &str = "warden:split-pane";

#[derive(serde::Deserialize)]
struct RectArg {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// All live window windows, behind a `Mutex`. Each `WindowState` holds a
/// `Registry` with `GhosttySurface: Send` values; all access is on the
/// main/UI thread (Tauri commands run there). [seam: manager only]
#[cfg(target_os = "macos")]
struct ManagerState(std::sync::Mutex<WindowManager>);

#[cfg(target_os = "macos")]
impl ManagerState {
    /// Lock the manager, recovering from a poisoned mutex. Surface-spawn failures no
    /// longer panic — they degrade to a cold tab (see `registry.rs` / `build_window`).
    /// What remains is the rare near-fatal AppKit failure: a multi-step op
    /// (`apply`/`materialize`) can still panic partway (e.g. an `ns_window`/window
    /// build `.expect`) and leave partial state, but recovering the guard keeps every
    /// subsequent command and the watcher reconcile alive instead of cascading one
    /// panic into permanently-dead IPC — the lesser evil.
    fn lock(&self) -> std::sync::MutexGuard<'_, WindowManager> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Holds the config-file watcher for the app's lifetime. The watcher stops
/// firing the moment it is dropped, so it must live in managed state rather
/// than as a local in `setup`. [seam: manager only]
#[cfg(target_os = "macos")]
struct WatcherState(#[allow(dead_code)] warden_config::Watcher);

/// Build and install the app menu: the shared spine (App/Config/Window submenus + the Close Tab
/// item) interleaved with warden's own Tab submenu. The digit chords still depend on `mode`, but
/// the items themselves now come from `shell_core::menu::build_tab_nav` — the mode's effect
/// (`Jump`: ⌘1–⌘9 jump straight to that 1-based tab position; `Cycle`: ⌘1/⌘2 alias next/previous,
/// reclaiming the digit-1/2 chords so jumps shift to ⌘3–⌘9) is defined there once, for all three
/// apps, not rebuilt here.
///
/// **⌘W closes a tab, ⌘⇧W closes the window — unchanged.** warden already had this right (the
/// family standard other apps are adopting); only the item/id/accelerator moved into
/// `shell_core::menu` so it can't drift per app. Reopen Last Closed (⌘⇧T) stays warden-only —
/// spliced into the spine's Window submenu below rather than added to the spine itself.
///
/// The on_menu_event handler is mode-agnostic — it keys on item IDs, and the
/// IDs simply differ per mode. `set_menu` replaces the app-global menu wholesale,
/// so a hot-reload that flips the mode just rebuilds (see the watcher).
#[cfg(target_os = "macos")]
fn build_app_menu(
    app: &tauri::AppHandle,
    mode: warden_config::TabDigitKeys,
    entries: Vec<crate::plan::WindowMenuEntry>,
    reopen_available: bool,
) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};

    let window_entries: Vec<shell_core::menu::WindowEntry> = entries
        .into_iter()
        .map(|e| shell_core::menu::WindowEntry {
            id: e.label,
            title: e.title,
            open: e.open,
            colour: Some(e.colour),
        })
        .collect();
    let config_path = warden_config::config_path();
    // About box carries the build stamp (shell_core::build_stamp → BUILD_GIT_SHA/BUILD_DATE) so a
    // glance confirms the installed app matches a given commit.
    let spine = shell_core::menu::build_spine(
        app,
        shell_core::menu::SpineConfig {
            app_name: "warden",
            config_path: &config_path,
            windows: &window_entries,
        },
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_GIT_SHA"),
        env!("BUILD_DATE"),
    )?;

    // The tab-nav block is shell-core's now: Previous/Next Tab, the ⌘1/⌘2 cycle aliases when the
    // mode asks for them, and the jump items (⌘1–9, or ⌘3–9 once the aliases take 1 and 2). Only
    // the submenu *composition* below is warden's.
    let nav = shell_core::menu::build_tab_nav(app, mode.is_cycle())?;
    let mut tab_menu = SubmenuBuilder::new(app, "Tab");
    for it in &nav.nav {
        tab_menu = tab_menu.item(it);
    }
    // Split (⌘D) — warden-only (see MENU_TAB_SPLIT's doc comment). Grouped with Close Tab / Pop
    // Out Tab as the other tab-scoped actions, ahead of the jump block below.
    let split_pane_item = MenuItemBuilder::with_id(MENU_TAB_SPLIT, "Split")
        .accelerator("CmdOrCtrl+D")
        .build(app)?;
    // The spine's Close Tab (⌘W) and Pop Out Tab (⌘⇧O) — warden's own semantics (unload the
    // active tab, NOT close the window) are unchanged; see the on_menu_event handler.
    tab_menu = tab_menu
        .separator()
        .item(&split_pane_item)
        .item(&spine.close_tab)
        .item(&spine.pop_out_tab)
        .separator();
    for it in &nav.jumps {
        tab_menu = tab_menu.item(it);
    }
    let tab_menu = tab_menu.build()?;

    // Reopen Last Closed (⌘⇧T) is warden-only (curator/lector have no equivalent), so it's not
    // part of the spine (YAGNI — add it there only if a sibling app wants it too). Spliced into
    // the spine's already-built Window submenu, at the top, mirroring warden's original layout
    // (reopen-last + separator, then the rest).
    let reopen_last = MenuItemBuilder::with_id(MENU_WINDOW_REOPEN_LAST, "Reopen Last Closed")
        .accelerator("Shift+Cmd+KeyT")
        .enabled(reopen_available)
        .build(app)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let window_menu = &spine.submenus[2];
    window_menu.insert_items(&[&reopen_last, &sep], 0)?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &spine.submenus[0], // App
            &tab_menu,
            &spine.submenus[1], // Config
            &spine.submenus[2], // Window
        ])
        .build()?;
    app.set_menu(menu)?;
    Ok(())
}

/// Re-derive the app menu from current manager state and install it. Locks
/// `ManagerState` itself, so callers MUST NOT hold the lock when calling this
/// (the mutex is non-reentrant). Rebuilds on launch, window open/close, and
/// hot-reload — the Window submenu's checkmarks/(closed) tags track live state.
#[cfg(target_os = "macos")]
fn rebuild_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::Manager;
    let st = app.state::<ManagerState>();
    let (mode, entries, reopen_available) = {
        let m = st.lock();
        (
            m.last_good.tab_digit_keys,
            m.window_menu_entries(),
            m.has_reopen_target(),
        )
    };
    build_app_menu(app, mode, entries, reopen_available)
}

/// Return the calling window's banner + tab descriptors, resolved by label.
#[cfg(target_os = "macos")]
#[tauri::command]
fn init_tabs(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) -> Option<InitDto> {
    state.lock().init_dto(window.label())
}

/// Probe this window's tabs once, on demand. The chrome calls this right after its
/// `warden:session-state` listener is registered, so the first session-presence emit
/// can't be lost to the listener-registration race — which matters most for
/// `probe_interval = 0` (no timer to heal a dropped emit) and also removes the
/// up-to-one-tick hollow-dot latency at startup for every interval.
#[cfg(target_os = "macos")]
#[tauri::command]
fn probe_now(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) {
    use tauri::Manager;
    // The chrome calls this once its `warden:session-state` listener is registered. Two deliveries,
    // together race-proof:
    //  1. Replay the last-known presence from the cache NOW. A probe pass that finished between this
    //     window's build (when `init_dto` snapshotted an empty cache → hollow dots) and the listener
    //     registering had its live per-tab emits dropped, and — `prev` populated — no later pass
    //     re-emits, so those dots would be stuck dark (the "launchpad dot never lights" bug, worst
    //     for a tab whose session pre-existed so its state never changes after that lost first pass).
    //  2. Bump so a live burst refreshes anything that changed since.
    let snapshot = state.lock().presence_cache.snapshot(window.label());
    probe::emit_presence_snapshot(window.app_handle(), window.label(), &snapshot);
    probe::bump(window.label());
}

/// What `activate_tab` reports back to the chrome. `live` is load-bearing (see below); `split` and
/// `focused` are Task 7's addition — the tab-switch path's own answer to "does this tab have a
/// second pane, and which one has keyboard focus", the one place `Registry::activate`'s
/// focus-preserving branch is ever checked (it's untestable in isolation — see the branch's own
/// doc comment). Wires `Registry::is_split`/`focused_pane` (dead since Task 3) — `split` also
/// short-circuits the `focused_pane` lookup below, since an unsplit tab's answer is trivially
/// always the primary.
#[cfg(target_os = "macos")]
#[derive(serde::Serialize)]
struct ActivateResult {
    live: bool,
    split: bool,
    focused: usize,
}

/// Activate tab `id` within the calling window's registry. **`live` reports whether the tab is
/// now live** — i.e. whether a surface actually covers the content hole; `split`/`focused` report
/// this tab's own pane layout (see `ActivateResult`).
///
/// `live` is load-bearing, not informational. warden's window is transparent and the
/// terminal NSView composites *above* the webview, so the hole is only ever filled by a live
/// surface; the chrome's `#empty-state` is the opaque backstop for when it isn't. A failed lazy
/// spawn leaves the tab cold **but still selected**, and the chrome cannot see that from the
/// `warden:error` banner alone — so if this returned nothing, the chrome would keep the placeholder
/// hidden (it keys on "a tab is selected") and the uncovered hole would leak the wallpaper: exactly
/// the leak the placeholder exists to stop. Report liveness so the chrome paints it.
#[cfg(target_os = "macos")]
#[tauri::command]
fn activate_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) -> ActivateResult {
    use tauri::Emitter;
    // Capture both the spawn outcome AND whether this call spawned a FRESH cold surface (whose
    // `initial_input` may be starting a session) for a probe-enabled tab — the two facts that
    // decide whether to arm a session-start await below. `split`/`focused` are this tab's OWN
    // layout, read under the SAME lock so they can't race a concurrent split/close. Both read
    // under the one lock.
    let (spawned_fresh, has_probe, err, split, focused) = {
        let mut m = state.lock();
        match m.windows.get_mut(window.label()) {
            Some(ws) => {
                let has_probe = ws.registry.tab_has_probe(&id);
                let (fresh, err) = match ws.registry.activate(&id) {
                    Ok(fresh) => (fresh, None),
                    Err(e) => (false, Some(e)),
                };
                let split = ws.registry.is_split(&id);
                // Trivially always the primary when unsplit — no need to ask `focused_pane` at all.
                let focused = if split {
                    ws.registry
                        .focused_pane(&id)
                        .map_or(0, registry::PaneIdx::index)
                } else {
                    0
                };
                (fresh, has_probe, err, split, focused)
            }
            None => (false, false, None, false, 0),
        }
    };
    // A lazy spawn failed on click: the tab stays cold (and selected — the chrome paints its
    // empty-state placeholder over the uncovered hole) instead of panicking. The chrome is
    // listening now, so push the reason to the banner.
    let live = err.is_none();
    if let Some(e) = err {
        eprintln!("warden: surface spawn failed for tab {id:?}: {e}");
        // A per-tab spawn error belongs to THIS window only. `emit` broadcasts to every
        // webview (the documented emit_to-leaks footgun), so stamp the window label into
        // the payload and let the chrome's label filter drop it in siblings — mirroring the
        // per-window build-time error (InitDto.error). The global config-error path keeps
        // emitting a bare string (no label), which every window's banner still shows.
        let _ = window.emit(
            "warden:error",
            serde_json::json!({
                "label": window.label(),
                "message": format!("couldn't open terminal: {e}"),
            }),
        );
    }
    // Fast-burst this window so a just-activated tab's dot is current within a pass, probing this tab
    // first so it lands in the first wave. If we just cold-spawned a probe-enabled tab, its
    // `initial_input` may be starting a session asynchronously — arm a session-start await so the
    // burst holds until it comes up (bounded by CAP), instead of settling "absent" within the
    // ~AGREE_TARGET×FAST window and only catching the session on the next slow poll. A warm switch
    // (or a probe-less tab) has no expected transition, so it's a plain priority bump.
    if spawned_fresh && has_probe {
        probe::bump_tab_await(window.label(), &id, true);
    } else {
        probe::bump_tab(window.label(), &id);
    }
    // Carried finding (Task 7): `Registry::activate` also brings a split tab's cold SECONDARY
    // live, silently — nothing before this told the chrome, so its `secondarySpawnedById` mirror
    // could go stale the moment a split tab's secondary finally spawns here (e.g. re-activating a
    // tab that was split while backgrounded). Push a fresh snapshot so the mirror catches up;
    // reuses the same init_dto→emit path every other cross-cutting refresh in this file already
    // uses, rather than inventing a narrower "just this tab's secondary" payload.
    //
    // Fix round 1 (IMPORTANT 2): gated on `split` — this used to run on EVERY activation, so
    // every ordinary (unsplit) tab click built a full DTO snapshot, emitted it, and drove the
    // chrome through a second `applyDto` → `sb.update()` → `setActiveId` → `reportRect` pass on
    // top of the one `onSelect` already does with THIS call's own `ActivateResult` — real IPC and
    // repaint cost on the hottest command in the app, for tabs that were never split at all. The
    // only thing this push exists to correct is `secondarySpawnedById` staleness for a tab that
    // HAS a secondary; an unsplit tab's `secondarySpawnedById` entry can't go stale (there's
    // nothing there to spawn), so gating out the common case changes nothing it needs to report.
    // Fix round 2 (finding 3): reuses `split` directly rather than a second `Registry::panes`
    // read — `Registry::panes` is deleted; it answered the identical `secondary.is_some()`
    // question `is_split` already had in hand, and keeping it alive purely to give a lint a
    // production call site was the wrong fix (see the fix report).
    if split {
        if let Some(dto) = state.lock().init_dto(window.label()) {
            let _ = window.emit("warden:refresh", dto);
        }
    }
    ActivateResult {
        live,
        split,
        focused,
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
    let mut m = state.lock();
    m.windows
        .get_mut(window.label())
        .and_then(|ws| ws.registry.unload(&id))
}

/// Split tab `id`: give it a second pane (the tab's own shell, no startup command) and bring it
/// live. `split` (Task 3) only *declares* the pane Cold; `activate` is what actually spawns and
/// shows it, so the pair is what makes the second terminal appear on screen. Splitting is
/// idempotent, so a double-fire from the chrome (e.g. a stray second ⌘D) can't produce a third
/// pane — it just re-activates an already-live one.
#[cfg(target_os = "macos")]
#[tauri::command]
fn split_pane(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) -> Result<(), String> {
    use tauri::Emitter;
    let result = {
        let mut m = state.lock();
        let ws = m
            .windows
            .get_mut(window.label())
            .ok_or("window not found")?;
        ws.registry.split(&id);
        ws.registry
            .activate(&id)
            .map(|_| ())
            .map_err(|e| format!("could not open the second pane: {e}"))
    };
    // `activate` attempts BOTH panes regardless of the primary's own Result (see its doc
    // comment) — the secondary's spawn outcome is exactly what the chrome's `secondarySpawnedById`
    // mirror needs and can't know on its own (it's best-effort/discarded here, same as anywhere
    // else that outcome is invisible). Push it on a "could not open the second pane" `Err` too,
    // same as `activate_tab`'s carried-finding refresh — the OTHER `Err` (`window not found`, the
    // `?` above) already returned out of the whole function before reaching here, but that's a
    // no-op regardless: `init_dto` fails the identical lookup, so there's nothing to refresh.
    if let Some(dto) = state.lock().init_dto(window.label()) {
        let _ = window.emit("warden:refresh", dto);
    }
    result
}

/// Drop tab `id`'s secondary pane (the divider ✕). Returns whether there was one — the chrome
/// only collapses the split layout (`setSplitVisible(false)`) on a genuine `true`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn close_pane(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) -> bool {
    let mut m = state.lock();
    match m.windows.get_mut(window.label()) {
        Some(ws) => ws.registry.close_secondary(&id),
        None => false,
    }
}

/// Point tab `id`'s keyboard focus at pane `pane` (0 = primary, 1 = secondary). Returns false
/// (no-op) if that pane doesn't exist — e.g. a stray click on a secondary that just closed.
#[cfg(target_os = "macos")]
#[tauri::command]
fn focus_pane(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    pane: usize,
) -> bool {
    let which = registry::PaneIdx::from_index(pane);
    let mut m = state.lock();
    match m.windows.get_mut(window.label()) {
        Some(ws) => ws.registry.focus_pane(&id, which),
        None => false,
    }
}

/// The close control on a divider in a popped-out window — shell-core's `close_hole { pane }`
/// contract, only ever invoked with 2+ holes, so `pane` names warden's secondary or nothing.
/// Same teardown and the same chrome tail as the scratch shell exiting there.
#[cfg(target_os = "macos")]
#[tauri::command]
fn close_hole(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, pane: usize) {
    use tauri::Manager;
    if registry::PaneIdx::from_index(pane) != registry::PaneIdx::Secondary {
        return;
    }
    let app = window.app_handle().clone();
    let closed = state.lock().close_detached_secondary(window.label());
    if let Some((origin, tab)) = closed {
        manager::announce_detached_secondary_closed(&app, window.label(), &origin, &tab);
    }
}

/// A popped-out tab's window opens at this size the FIRST time that tab is popped — a single
/// terminal needs far less than a full multi-tab window's config-resolved default (1500×1000).
///
/// Only the first time: shell-core's geometry plugin persists a detached window's size and
/// position keyed on its (deterministic, per-tab) `shell-detach:` label, and restores over this
/// size inside `open_detached`'s `build()`. So this pair is the no-memory default, never a
/// description of how big the window will actually be — see the birth-rect comment in
/// `pop_out_tab`, which is exactly where reading it as the latter goes wrong.
#[cfg(target_os = "macos")]
const DETACHED_DEFAULT_WIDTH: f64 = 900.0;
#[cfg(target_os = "macos")]
const DETACHED_DEFAULT_HEIGHT: f64 = 640.0;
/// The detached window's banner height (matches `detach.html`'s `#banner`, 2.25rem ≈ 36px).
/// Only used to size the surface's BIRTH rect so it doesn't flash full-height for one frame
/// before `detach.html`'s own `set_hole_rect` lands and reports the exact hole.
#[cfg(target_os = "macos")]
const DETACHED_BANNER_H: f64 = 36.0;
/// Width of the drag divider `detach.html` puts between two holes (its `.divider` rule, 6px).
/// Same standing as `DETACHED_BANNER_H` and used for the same one thing: a birth rect close
/// enough that a split pop-out doesn't flash mis-sized for the frame before the page's own
/// `set_hole_rect` reports the exact holes. Never a layout authority — the page is.
#[cfg(target_os = "macos")]
const DETACHED_DIVIDER_W: f64 = 6.0;
/// The divider ratio a split pop-out falls back to when the chrome supplies none (or supplies
/// a non-finite one): the same even split `detach.html` would lay out unasked.
#[cfg(target_os = "macos")]
const DETACHED_DEFAULT_RATIO: f64 = 0.5;

/// The divider ratio a split pop-out lays out at, from the chrome's advisory `ratio`
/// argument: absent or non-finite → the even fallback, otherwise clamped to the same
/// `0.1..=0.9` band the chrome's own drag and `detach.html`'s drag both clamp to, so a
/// popped-out split can never open at a width neither of them would let you drag it to.
///
/// One home for that decision, called twice on the pop-out path (the payload's `panes`, and
/// the birth rects) — they must agree, and the second frame would silently paper over it if
/// they didn't.
#[cfg(target_os = "macos")]
fn split_ratio(ratio: Option<f64>) -> f64 {
    match ratio {
        Some(r) if r.is_finite() => r.clamp(0.1, 0.9),
        _ => DETACHED_DEFAULT_RATIO,
    }
}

/// Pop tab `id` out of the calling window into its own detached window, preserving the live
/// terminal(s) (surface + PTY). The tab stays present in the origin's sidebar as a `Detached`
/// placeholder (rendered with the ⤢ mark) until the detached window closes, at which point
/// `redock` (wired via `shell_core::detach::wire_return`) moves it back.
///
/// **Pop-out is whole-tab**, so a split tab takes BOTH panes: `detach` hands back the pair,
/// `DetachSpec.panes` asks the shared detach page for two holes at the origin's own divider
/// ratio, and each hole's `set_hole_rect` routes to its own surface. `ratio` is that divider
/// ratio, supplied by the chrome (which owns split geometry — Rust never stores one); it is
/// advisory, and how many holes the window gets is decided by how many surfaces actually came
/// back, never by `ratio` or by `is_split`. An unsplit tab is unchanged in every respect: one
/// surface, `panes: vec![]`, and a payload byte-identical to the pre-panes one.
///
/// **Lock discipline (load-bearing):** the `ManagerState` lock is held ONLY to extract the
/// surface + read the spec inputs (phase 1) and again to store the result (phase 3). The window
/// build + reparent (phase 2) run with the lock RELEASED — `open_detached` builds a window and
/// its `birth_content` closure moves the surface in, and holding the lock across that (which can
/// re-enter Tauri/AppKit) risks deadlock. The surface is threaded through an `Option` so the
/// closure can borrow it and we can recover it afterward on either outcome.
#[cfg(target_os = "macos")]
#[tauri::command]
fn pop_out_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    ratio: Option<f64>,
) -> Result<(), String> {
    use tauri::{Emitter, Manager};
    let app = window.app_handle().clone();
    let origin_label = window.label().to_string();

    // Phase 1 — under the lock: extract the live surface(s) (leaving a Detached placeholder in
    // each slot) and read the banner spec inputs. Lock dropped at the end of this block.
    let (surface, secondary, spec, token) = {
        let mut m = state.lock();
        let ws = m
            .windows
            .get_mut(&origin_label)
            .ok_or_else(|| "window not found".to_string())?;
        // Bring a never-opened (cold) tab live first, so popping the ⤢ affordance — which the
        // sidebar shows on EVERY row, not just loaded ones — works from any tab rather than
        // erroring on a cold one. Spawning happens in the origin window; `detach` then extracts
        // the now-live surface and phase 2 reparents it out (all synchronous, so the brief
        // in-origin overlap never renders). Already-Detached / unknown tabs stay a clean error.
        ws.registry
            .ensure_spawned_by_id(&id)
            .map_err(|e| format!("could not open the tab to pop it out: {e}"))?;
        let (surface, secondary) = ws
            .registry
            .detach(&id)
            .ok_or_else(|| "tab is not available to pop out".to_string())?;
        let title = ws.registry.tab_title(&id).unwrap_or_else(|| id.clone());
        let colour = ws.colour.clone();
        // Two holes iff two surfaces actually came back. Keyed on the surfaces, not on
        // `is_split`: a split tab whose secondary was cold detaches with one surface, and a
        // second hole with nothing behind it is a transparent leak straight to the desktop.
        let panes = if secondary.is_some() {
            let r = split_ratio(ratio);
            vec![r, 1.0 - r]
        } else {
            // The unsplit case, which is every tab that has never been split: empty, so the
            // page lays out one undivided hole and shell-core omits `panes` from the payload
            // entirely — byte-identical to the pre-panes emission.
            vec![]
        };
        let spec = shell_core::detach::DetachSpec {
            title,
            colour: Some(colour),
            width: DETACHED_DEFAULT_WIDTH,
            height: DETACHED_DEFAULT_HEIGHT,
            panes,
        };
        let token = crate::plan::detach_window_token(&origin_label, &id);
        (surface, secondary, spec, token)
    };

    // Phase 2 — lock RELEASED: build the detached window; birth_content reparents the surface
    // into it. Birth rect ≈ the hole below the banner (detach.html corrects it on load).
    //
    // `size` is the BUILT window's real size, handed in by `open_detached` — never `spec`'s / the
    // DETACHED_DEFAULT_* constants it was built with. shell-core's geometry plugin restores this
    // tab's remembered size and position during `build()`, so for every pop-out after the first
    // those constants are stale by the time this closure runs. It arrives in logical points, which
    // is what the NSView frame wants.
    //
    // BOTH panes move here, and every way out of this closure leaves both surfaces reachable:
    // a failed primary reparent has moved nothing, and a failed secondary reparent (only
    // possible for an already-closed surface — the window lookups it shares with the primary
    // have just succeeded) puts the primary back in the origin before returning, so
    // `open_detached`'s `window.close()` never takes a live view down with it. `origin_nsw`
    // is resolved HERE, before the build, because that rollback needs it and the closure has
    // only the new window; `None` (the origin can't report an NSWindow, which is already a
    // broken origin) costs the rollback its destination, so the closure refuses up front in
    // exactly the case that would need one — see its own guard — rather than moving the
    // primary into a window it could not move it back out of. Either way the surfaces and
    // their PTYs survive into the registry below, which is the guarantee that matters.
    let origin_nsw = window.ns_window().ok();
    let mut surface_opt = Some(surface);
    let mut secondary_opt = secondary;
    let build = shell_core::detach::open_detached(&app, &token, &spec, "warden", |win, size| {
        let nsw = win.ns_window()?;
        let hole_h = (size.height - DETACHED_BANNER_H).max(0.0);
        // One hole, or the two the page divides by the same ratio `spec.panes` carries. Both
        // are corrected by `detach.html`'s own `set_hole_rect` on load; these only keep the
        // first frame from being visibly wrong.
        let (primary_rect, secondary_rect) = match secondary_opt.is_some() {
            false => (
                crate::surface::PixelRect {
                    x: 0.0,
                    y: 0.0,
                    width: size.width,
                    height: hole_h,
                },
                None,
            ),
            true => {
                let r = split_ratio(ratio);
                let avail = (size.width - DETACHED_DIVIDER_W).max(0.0);
                let left = avail * r;
                (
                    crate::surface::PixelRect {
                        x: 0.0,
                        y: 0.0,
                        width: left,
                        height: hole_h,
                    },
                    Some(crate::surface::PixelRect {
                        x: left + DETACHED_DIVIDER_W,
                        y: 0.0,
                        width: avail - left,
                        height: hole_h,
                    }),
                )
            }
        };
        // Refuse BEFORE moving anything when the rollback below has no destination: the
        // secondary's failure arm needs `origin_nsw` to put the primary back, so with a
        // secondary in hand and no origin NSWindow the only reachable failure is the one that
        // strands the primary's view inside a window `open_detached` then closes. Bailing here
        // costs nothing that isn't already lost (an origin that can't report an NSWindow is
        // broken, and both surfaces still survive into the rollback registry below), and it
        // buys the cheap guarantee that no branch past this point moves a view it cannot move
        // back. Unsplit is untouched — with no secondary there is no second move to fail.
        if origin_nsw.is_none() && secondary_opt.is_some() {
            return Err(std::io::Error::other(
                "origin window has no NSWindow to roll a split pop-out back into",
            )
            .into());
        }
        surface_opt
            .as_mut()
            .expect("surface present during birth")
            .reparent(nsw as *mut std::os::raw::c_void, primary_rect)
            .map_err(|e| std::io::Error::other(format!("reparent failed: {e}")))?;
        if let (Some(s), Some(rect)) = (secondary_opt.as_mut(), secondary_rect) {
            if let Err(e) = s.reparent(nsw as *mut std::os::raw::c_void, rect) {
                // Undo the primary's move: this window is about to be closed by
                // `open_detached`, and a view left inside it would be a live PTY with
                // nothing on screen. The rect is a placeholder — the `activate` on the
                // failure path below re-asserts the origin's real hole rects.
                if let Some(o) = origin_nsw {
                    let _ = surface_opt
                        .as_mut()
                        .expect("surface present during birth")
                        .reparent(o as *mut std::os::raw::c_void, manager::INITIAL_RECT);
                }
                return Err(std::io::Error::other(format!(
                    "reparent of the second pane failed: {e}"
                ))
                .into());
            }
        }
        Ok(())
    });

    let label = match build {
        Ok(label) => label,
        Err(e) => {
            // The window build / reparent failed. Both surfaces are intact and parented to
            // the ORIGIN window (reparent only errors before it moves a view, and the
            // secondary's failure arm above moves the primary back) — so put BOTH back into
            // the origin's registry slot and the tab stays live, never a permanent Detached
            // placeholder with no window. A partial restore here would leave a live PTY with
            // no window, no slot, and no route back. If the origin vanished meanwhile, both
            // are closed here (the tab genuinely has no home).
            let surface = surface_opt.take().expect("surface held on build failure");
            let secondary = secondary_opt.take();
            let mut m = state.lock();
            if let Some(ws) = m.windows.get_mut(&origin_label) {
                match ws.registry.attach(&id, surface, secondary) {
                    // Re-assert the origin's CURRENT selection (not this tab — pop-out can be
                    // driven on a background tab, and `activate` would move the shown surface
                    // out from under the sidebar's highlight). This is what re-applies the
                    // real hole rects over the placeholder ones a rollback reparent used.
                    Ok(()) => {
                        if let Some(active) = ws.registry.active_tab().map(str::to_string) {
                            let _ = ws.registry.activate(&active);
                        }
                    }
                    Err((p, s)) => manager::close_both(p, s),
                }
            }
            return Err(format!("couldn't pop out tab: {e}"));
        }
    };

    // Phase 3 — re-lock to store the (now reparented) surfaces + wire the return.
    let surface = surface_opt.take().expect("surface reparented on success");
    let secondary = secondary_opt.take();
    {
        let mut m = state.lock();
        m.detached.insert(
            label.clone(),
            manager::DetachedSurface {
                surface,
                secondary,
                origin_label: origin_label.clone(),
                tab_id: id.clone(),
            },
        );
    }
    {
        let app2 = app.clone();
        let label2 = label.clone();
        // wire_return resolves the detached window by label itself (get_window) — warden's stays
        // single-webview (it hosts a re-parented native surface), but the shared lookup is correct
        // either way, so callers no longer pick get_webview_window (wrong for curator/lector).
        shell_core::detach::wire_return(&app, &label, move || redock(&app2, &label2));
    }

    // The origin's row now renders detached; refresh it. sync_empty_surface is a no-op here
    // (origin still open) but keeps the home-surface authority single-sourced; the menu is
    // unchanged (detached windows aren't in it) but rebuilt for consistency.
    {
        let mut m = state.lock();
        m.sync_empty_surface(&app);
    }
    let _ = rebuild_menu(&app);
    if let Some(dto) = state.lock().init_dto(&origin_label) {
        let _ = app.emit_to(origin_label.as_str(), "warden:refresh", dto);
    }
    Ok(())
}

/// Raise the detached window hosting tab `id` (popped out of the calling window). The chrome
/// calls this instead of `activate_tab` when a *detached* row is clicked in the origin sidebar —
/// there is no local surface to activate, so "select" means "bring its window forward".
#[cfg(target_os = "macos")]
#[tauri::command]
fn raise_popped_window(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) {
    use tauri::Manager;
    let app = window.app_handle().clone();
    let label = state.lock().detached_label_for(window.label(), &id);
    if let Some(label) = label {
        if let Some(win) = app.get_webview_window(&label) {
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    }
}

/// Dock tab `id` back into its origin window — the ↩ pop-in overlay on a detached row. Closing the
/// detached window fires its `Destroyed` handler (`redock`), which returns the tab here; this reuses
/// the exact same return path as the user closing the popped-out window by hand. No-op if `id`
/// isn't a tab detached from the calling window.
#[tauri::command]
fn pop_in_tab(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    use tauri::Manager;
    let app = window.app_handle().clone();
    let label = state.lock().detached_label_for(window.label(), &id);
    if let Some(label) = label {
        if let Some(win) = app.get_webview_window(&label) {
            let _ = win.close();
        }
    }
}

/// Return a popped-out tab to its origin when its detached window closes — the `on_close`
/// wired by `shell_core::detach::wire_return`. Runs on the main thread (Tauri delivers the
/// window `Destroyed` event there), so it can lock `ManagerState` and touch AppKit directly.
/// Captures only the `AppHandle` (Clone) + the detached label; the state is fetched inside.
#[cfg(target_os = "macos")]
fn redock(app: &tauri::AppHandle, detached_label: &str) {
    use tauri::{Emitter, Manager};
    let st = app.state::<ManagerState>();
    let origin = st.lock().redock(app, detached_label);
    let Some(origin_label) = origin else {
        return; // already redocked (double-close) — nothing to do
    };
    // The origin may have been reopened by redock; rebuild the menu so its checkmark/(closed)
    // tag is right (lock already released — rebuild_menu re-locks).
    let _ = rebuild_menu(app);
    // Push a fresh snapshot so the origin's chrome un-detaches the returned tab's row and shows
    // it active. A no-op emit if the origin is gone (tab had no home).
    let dto = st.lock().init_dto(&origin_label);
    if let Some(dto) = dto {
        let _ = app.emit_to(origin_label.as_str(), "warden:refresh", dto);
    }
    // The tab's session may have changed while popped out — fast-burst the origin's dots.
    probe::bump(&origin_label);
}

/// Kill the *session* tab `id` represents (the thing its `probe` checks for) by running
/// its configured `kill` command via `sh -c`, cwd = the tab's dir, fire-and-forget on a
/// detached thread (exit code ignored — warden has no response to a failed kill, and must
/// not block the UI thread). Does NOT unload warden's terminal surface: a live tab stays
/// live. No-op if the tab has no `kill` set. After the kill completes, bump this window so
/// the scheduler fast-bursts and the cyan presence dot drops promptly once the session is
/// actually gone. Same minimal-env PATH footgun as probes — see scrub note + CLAUDE.md.
#[cfg(target_os = "macos")]
#[tauri::command]
fn kill_session(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    let target = {
        let m = state.lock();
        m.windows
            .get(window.label())
            .and_then(|ws| ws.registry.kill_target(&id))
    };
    let Some((dir, title, cmd)) = target else {
        return; // unknown tab or no kill command configured
    };
    let cmd = probe::substitute(&cmd, &dir, &title);
    let label = window.label().to_string();
    // Run the kill fire-and-forget off the UI thread, then bump this window so the scheduler's fast
    // burst catches the leave-Present transition the moment teardown lands. `bump_tab_await(false)`
    // arms a directional await (session should go DOWN) so the burst holds Fast until it does rather
    // than settling on the still-Present state and dropping to the slow poll — the fix for the
    // ~5s-lingering dot when `amux --kill` returns before tmux teardown fully lands. No optimistic
    // drop and no suppression flag: the dot tracks `warden:session-state` and converges
    // monotonically (no off→on→off flicker). A genuinely-failed kill leaves the session present, so
    // the await simply rides to CAP and the dot correctly stays lit.
    std::thread::spawn(move || {
        let _ = probe::run_probe(&cmd, &dir);
        // Probe THIS tab first in the burst so its dot clears within one probe, not after every
        // other tab in a wide window's sweep.
        probe::bump_tab_await(&label, &id, false);
    });
}

/// Restart the *session* tab `id` represents by re-typing its startup `cmd` into the live shell
/// (see `Registry::start_session` — the runtime twin of spawn-time `initial_input`). No-op if the
/// tab is cold or has no `cmd`. The started session appears **asynchronously** — the shell has to
/// run the typed command — so a single ordinary burst can settle "absent" before it comes up. A
/// directional await (`bump_tab_await(true)`, session should come UP) holds the burst Fast until the
/// session is actually `Present` (bounded by CAP), so the dot lights within a probe of the session
/// landing rather than up to a slow poll later. Same minimal-env PATH footgun as probes.
#[cfg(target_os = "macos")]
#[tauri::command]
fn start_session(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    let started = {
        let m = state.lock();
        m.windows
            .get(window.label())
            .map(|ws| ws.registry.start_session(&id))
            .unwrap_or(false)
    };
    if !started {
        return; // cold / no cmd / unknown — nothing typed, nothing to re-probe
    }
    // Probe this tab first in the burst so its dot lights as soon as the session comes up, not after
    // the rest of a wide window's sweep; the await keeps the burst alive across the async start.
    probe::bump_tab_await(window.label(), &id, true);
}

/// Re-scan every project-tree root of the focused window's config and reconcile:
/// projects that appeared/vanished on disk surface as tab add/remove, without a
/// config-file edit. Diffs the freshly-expanded config against what's on screen
/// (`last_good`), so it is NOT a no-op when disk changed.
#[cfg(target_os = "macos")]
#[tauri::command]
fn rescan_root(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) {
    use tauri::Manager;
    let app = window.app_handle().clone();
    // Clone the raw config under a brief lock, then run the recursive scan OFF the
    // lock so it never stalls the background probe thread; re-lock only to apply.
    // Sync `#[tauri::command]`s run on the main thread (as does hot-reload), and we
    // don't yield the main thread between the two locks, so no other writer can
    // interleave — `last_good`/`raw_config` are unchanged when we re-lock.
    let raw = state.lock().raw_config.clone();
    let fresh = manager::effective_config(&raw);
    {
        let mut m = state.lock();
        let recon = warden_config::reconcile(&m.last_good, &fresh);
        m.apply(&app, &recon, &fresh);
        m.last_good = fresh;
    } // release the ManagerState lock before the lock-free bump
      // New discovered tabs may carry probes — fast-burst every window so their dots populate now.
    probe::bump_all(&app);
}

/// Update the calling window's pane frame from a web-coordinate rect. `pane` is the hole's
/// pane index: `None`/`Some(0)` = primary, `Some(1)` = secondary.
#[cfg(target_os = "macos")]
#[tauri::command]
fn set_hole_rect(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    rect: RectArg,
    pane: Option<usize>,
) {
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
    // inner_size is in physical pixels; divide by scale to get points. A rect report
    // can race window teardown (the window's gone but a queued JS call still fires),
    // so bail rather than panic — consistent with the scale_factor fallback above.
    let Ok(size) = window.inner_size() else {
        return;
    };
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

    // None is the single-hole caller (shell-core's detach.html when it declares no panes, and
    // any older chrome); anything unexpected floors to the primary (`from_index`) rather than
    // erroring, so a bad index can't leave a hole unpositioned and transparent.
    let which = crate::registry::PaneIdx::from_index(pane.unwrap_or(0));

    let mut m = state.lock();
    if let Some(ws) = m.windows.get_mut(window.label()) {
        ws.registry.set_pane_frame(which, view_rect);
    } else {
        // A detached (popped-out) window lives outside `windows`; its banner-shell page
        // (`detach.html`) reports its own hole rect via this same command, so route it to
        // the detached surface. No-op if the label is neither a real nor a detached window.
        m.set_detached_frame(window.label(), which, view_rect);
    }
}

/// warden's starter config, offered by the shared home surface's "Create a starter config"
/// button when no config file exists at all. Tracked, and `include_str!`'d so a missing/renamed
/// template is a build error rather than a runtime surprise.
#[cfg(target_os = "macos")]
const DEFAULT_CONFIG: &str = include_str!("default-config.toml");

/// The home surface's "Create a starter config" button. This is where config-core is called (via
/// `warden_config`'s re-export — this crate never pins config-core directly, the same
/// one-source-of-truth rule as its other re-exported house helpers) — shell-core owns the
/// surface but never touches config-core (the cores stay independent).
#[cfg(target_os = "macos")]
#[tauri::command]
fn shell_home_create_config(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let path = warden_config::config_path();
    match warden_config::write_default_config(&path, DEFAULT_CONFIG) {
        // A config already existed — say so rather than report a success that didn't happen.
        Ok(false) => Err(format!(
            "{} already exists — left untouched",
            path.display()
        )),
        Ok(true) => match warden_config::load_with(&path, &login_shell()) {
            Ok(loaded) => {
                // Expand `[[window.root]]`s into the EFFECTIVE config BEFORE taking the lock —
                // the walk is slow and must never run under the ManagerState mutex, or it stalls
                // the background probe thread for its whole duration (same discipline as
                // `rescan_root` and the hot-reload watcher below).
                let effective = manager::effective_config(&loaded.config);
                let st = app.state::<ManagerState>();
                let mut m = st.lock();
                m.materialize_effective(&app, loaded.config, effective);
                drop(m); // release before rebuild_menu (non-reentrant mutex)
                let _ = rebuild_menu(&app);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// The home surface's "Edit Config" button (shown for a config that failed to load). Reuses the
/// spine's own Edit Config action rather than a second `open` spawn.
#[cfg(target_os = "macos")]
#[tauri::command]
fn shell_home_edit_config() {
    let path = warden_config::config_path();
    shell_core::menu::handle_spine_event(shell_core::menu::ids::EDIT_CONFIG, &path);
}

/// The home surface's per-window button (shown for the `Windows` list state): open, or focus if
/// already open, then update the empty-surface (it recedes once a real window exists) and
/// rebuild the Window menu. Mirrors the Window-menu `on_menu_event` open/focus path — same
/// invariant as `MENU_WINDOW_REOPEN_LAST`'s handler: every window-open path must sync, not just
/// the home surface's own click.
#[cfg(target_os = "macos")]
#[tauri::command]
fn shell_home_open_window(id: String, app: tauri::AppHandle) {
    use tauri::Manager;
    let st = app.state::<ManagerState>();
    {
        let mut m = st.lock();
        if m.windows.contains_key(&id) {
            m.focus_window(&id);
        } else {
            m.reopen_window(&app, &id);
        }
        m.sync_empty_surface(&app);
    } // release the lock before rebuild_menu (non-reentrant mutex)
    let _ = rebuild_menu(&app);
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

/// Make libghostty find *warden's* ghostty resources — the terminfo it needs to call itself a
/// ghostty terminal. libghostty locates them by climbing from the running executable for the
/// sentinel `Contents/Resources/terminfo/78/xterm-ghostty` (`resourcesdir.zig`); miss it and it
/// silently exports `TERM=xterm-256color` instead of `xterm-ghostty` (`termio/Exec.zig`). That
/// fallback is not cosmetic: `xterm-256color` advertises no `Sync` capability, so tmux stops
/// bracketing its redraws in DEC mode 2026 — the one signal that makes libghostty *pause*
/// rendering — so it renders half-drawn frames, and an unfocused surface (which paints its hollow
/// cursor on **every** frame, ahead of the blink gate) flickers that cursor wherever a mid-repaint
/// sample left it. Two cases, one lever — `GHOSTTY_RESOURCES_DIR`, which libghostty honours ahead
/// of the climb and from which it derives `TERMINFO` as `<parent>/terminfo`:
///
/// * **Packaged `warden.app`** — the climb succeeds on its own (the bundle ships the resources), so
///   we only *clear* an inherited `GHOSTTY_RESOURCES_DIR`. warden is routinely launched from inside
///   another ghostty/warden terminal, which exports that var; inherited, it would point warden at a
///   *different* Ghostty build's terminfo. Same reasoning as the `$TMUX` scrub: a terminal host must
///   not inherit its launcher's terminal context.
/// * **Unbundled (`cargo run` / `just run`)** — there is no bundle to climb to, so point it at the
///   vendored resources. Without this, dev runs would silently be `xterm-256color` and reproduce
///   bugs the shipped app doesn't have (and mask ones it does). Debug-only, so no build-machine path
///   is ever baked into a release binary.
///
/// Must run before libghostty is initialised — the resources dir is resolved once at app init.
fn configure_ghostty_resources() {
    // The bundle's own resources must win over whatever terminal launched us.
    std::env::remove_var("GHOSTTY_RESOURCES_DIR");

    #[cfg(debug_assertions)]
    {
        let vendor = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/resources"));
        if vendor.join("terminfo/78/xterm-ghostty").exists() {
            std::env::set_var("GHOSTTY_RESOURCES_DIR", vendor.join("ghostty"));
        } else {
            eprintln!(
                "warden: vendored ghostty resources missing at {} — terminals will fall back to \
                 TERM=xterm-256color (no synchronized output). Run `just revendor-ghostty`.",
                vendor.display()
            );
        }
    }
}

/// The shell warden spawns when a tab's config sets none — the user's **login shell**, run
/// as a login shell, exactly as a terminal does. Read from `$SHELL` (launchd populates it from
/// the user's directory record even for a Dock/Finder launch), falling back to the macOS
/// default. Returned as an absolute path with `-l`, which is the whole point: libghostty finds
/// it without any PATH lookup — a GUI launch's minimal launchd PATH (`/usr/bin:/bin:/usr/sbin:/sbin`)
/// would otherwise miss a Homebrew/nix shell and the tab would die `exec: <shell>: not found` —
/// and the login shell then sources the user's config and builds PATH for the interactive
/// session. A config `shell` (at any cascade level) overrides this; warden is generic, so an
/// override is an arbitrary command. A bare-name override (`fish -l`) resolves against the
/// login-shell PATH adopted by `restore_login_path` at startup; an absolute path remains the
/// robust fallback for a binary that lives only on an interactive-only PATH.
fn login_shell() -> String {
    let path = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    format!("{path} -l")
}

// Sentinels bracketing the login PATH in the helper-shell output, so anything an rc file
// prints around it (banners, `nvm`/`conda` chatter) can't corrupt the readout.
const PATH_SENTINEL_START: &str = "__WARDEN_PATH_START__";
const PATH_SENTINEL_END: &str = "__WARDEN_PATH_END__";

/// Adopt the user's **login-shell PATH** so warden — and every terminal, `probe`, and `kill`
/// it spawns — can find tools the GUI launch context hides. Launched from Dock/Finder/Spotlight,
/// a `.app` inherits only the minimal launchd PATH (`/usr/bin:/bin:/usr/sbin:/sbin`); a shell,
/// probe, or kill command named without an absolute path (`fish -l` as a config override, bare
/// `tmux` in a probe) is then not found and silently fails — a `shell` override dies on spawn
/// (`exec: fish: not found`), a probe reports "no session." The built-in *default* shell sidesteps
/// this by construction (absolute `$SHELL -l`, see `login_shell`), but config-supplied commands are
/// arbitrary and routinely bare, so they need the PATH. Rather than guess install prefixes —
/// Homebrew, nix, MacPorts and custom setups all differ — we ask the user's own login shell what
/// PATH it builds and adopt that, the approach VS Code and `exec-path-from-shell` use for the same
/// GUI-launch gap. Since surfaces/probes/kill all inherit warden-app's process env (same lever as
/// the tmux scrub), setting it once here fixes all three.
///
/// Best-effort and self-limiting: if the shell can't be run, exceeds the deadline, or yields
/// nothing parseable, PATH is left exactly as inherited — warden never ends up *worse* off than a
/// no-op. Captures the **login** environment (`-l`); a PATH set only in interactive-only rc
/// (`.zshrc` without a `.zprofile` export) isn't seen — naming such a binary by absolute path in
/// config remains the robust fallback.
fn restore_login_path() {
    use std::sync::mpsc;
    use std::time::Duration;

    // launchd populates SHELL from the user's directory record even for GUI launches; fall back
    // to the macOS default if it's somehow unset.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // Read PATH via `printenv`, NOT `echo $PATH`: the latter is shell-syntax-dependent (fish joins
    // list vars with spaces, not colons). `printenv` emits the colon-delimited PATH the login shell
    // built regardless of which shell ran it, so one snippet works for bash/zsh/fish alike.
    let snippet = format!(
        "printf %s {PATH_SENTINEL_START}; /usr/bin/printenv PATH; printf %s {PATH_SENTINEL_END}"
    );

    // Run on a side thread with a deadline so a slow/pathological login rc (conda, nvm, …) can't
    // hang warden's startup. On timeout we abandon the result; the orphan child reaps itself and
    // PATH stays as-is.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(&shell)
            .args(["-l", "-c", &snippet])
            .output();
        let _ = tx.send(out);
    });

    let Ok(Ok(out)) = rx.recv_timeout(Duration::from_secs(3)) else {
        return; // failed to spawn or timed out — keep the inherited PATH
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(path) = extract_sentinel_path(&stdout) {
        if !path.is_empty() {
            std::env::set_var("PATH", path);
        }
    }
}

/// Extract the PATH the login shell printed between the sentinels, tolerating rc-file noise
/// printed before or after it. `None` if both sentinels aren't present (e.g. the shell errored
/// or printed nothing), which the caller treats as "leave PATH untouched".
fn extract_sentinel_path(output: &str) -> Option<String> {
    let after = output.split_once(PATH_SENTINEL_START)?.1;
    let inner = after.split_once(PATH_SENTINEL_END)?.0;
    Some(inner.trim().to_string())
}

fn main() {
    // warden hosts terminals — it must not leak its own launcher's tmux membership into them
    // (breaks nested agentmux/tmux). Scrub before anything else inherits the environment.
    scrub_inherited_tmux_env();
    // A Dock/Finder/Spotlight launch gives warden only the minimal launchd PATH, so a config
    // `shell`/`probe`/`kill` named by bare command would be not-found. Adopt the login-shell PATH
    // before any surface or probe spawns and inherits this process's environment.
    restore_login_path();
    // Point libghostty at warden's own ghostty resources, so terminals get TERM=xterm-ghostty (and
    // with it synchronized output) rather than the silent xterm-256color fallback. Before ghostty
    // init: the resources dir is resolved once, at app init.
    configure_ghostty_resources();

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

    // Register the shell-core plugins (geometry + updater + process) — the set every sibling app
    // installs identically. The geometry plugin persists each window's size/position (AppKit
    // points, clamped to its monitor's work area on restore) keyed by Tauri label *within a
    // per-config store* (scoped by shell-core's `geometry_filename` from the config path below) so
    // two configs sharing a window title don't share bounds; it restores on its own `on_window_ready`
    // hook, which fires for runtime-built windows too — nothing here triggers it. That covers a
    // popped-out tab's window as well, keyed by its `shell-detach:` label (deterministic per tab,
    // see `plan::detach_window_token`), so a re-popped tab reopens at the size and position it was
    // left at. Only the shared home surface is excluded, structurally inside the plugin itself, so
    // `skip_labels` carries none of warden's windows here.
    let config_path = warden_config::config_path();
    shell_core::register_plugins(tauri::Builder::default(), Some(&config_path), &[])
        // Menu items act on the focused window. Tab nav (⌘⇧[/⌘⇧], ⌘1–⌘9) and Close Tab (⌘W)
        // route through its chrome, which owns the tab list + select()/unload. emit_to is NOT a
        // reliable per-window target here (it leaks to siblings — the same reason warden:refresh
        // carries a label, see manager.rs), so every payload carries the focused window's `label`
        // and the chrome ignores events not addressed to it. Close Window (⌘⇧W) closes it
        // directly. Unknown IDs (e.g. predefined Quit/Minimize, which self-handle) are ignored.
        .on_menu_event(|app, event| {
            use tauri::{Emitter, Manager};
            let id = event.id().as_ref();

            // The spine's file-acting ids (Edit Config, Reveal Config) need no window — let it
            // consume them first. config_path() is WARDEN_CONFIG else ~/.config/warden/config.toml.
            let cfg_path = warden_config::config_path();
            if shell_core::menu::handle_spine_event(id, &cfg_path) {
                return;
            }

            // Window menu acts on the manager/app, not the focused window — handle it
            // before the focused-window lookup (reopen-last needs no focused window).
            if id == MENU_WINDOW_REOPEN_LAST {
                let st = app.state::<ManagerState>();
                let reopened = {
                    let mut m = st.lock();
                    let reopened = m.reopen_last(app);
                    // Reopening takes the live set from zero→≥1, so close the home surface if
                    // it was showing (⌘⇧T is reachable while it's the front surface). Same
                    // invariant `shell_home_open_window` upholds — every window-open path must
                    // sync, not just the home surface's own click.
                    if reopened {
                        m.sync_empty_surface(app);
                    }
                    reopened
                };
                if reopened {
                    let _ = rebuild_menu(app);
                }
                return;
            }
            if let Some(win_label) = shell_core::menu::selected_window(id) {
                let st = app.state::<ManagerState>();
                {
                    let mut m = st.lock();
                    if m.windows.contains_key(win_label) {
                        m.focus_window(win_label);
                    } else {
                        m.reopen_window(app, win_label);
                    }
                    // Opening a closed window from the Window menu while the home surface is
                    // showing (zero real windows) must close it — the same invariant
                    // `shell_home_open_window` upholds. Harmless no-op on the focus (already-open)
                    // path, since the home surface can't be showing then.
                    m.sync_empty_surface(app);
                }
                let _ = rebuild_menu(app);
                return;
            }

            let Some(win) = app
                .webview_windows()
                .into_values()
                .find(|w| w.is_focused().unwrap_or(false))
            else {
                return;
            };
            let label = win.label().to_string();
            // Tab navigation (⌘⇧[ / ⌘⇧] , ⌘1–9, and the ⌘1/⌘2 cycle aliases) — shell-core routes the
            // id, so this handler is mode-blind: the aliases arrive as plain Next/Prev.
            if let Some(action) = shell_core::menu::tab_nav_action(id) {
                use shell_core::menu::TabNavAction;
                match action {
                    TabNavAction::Prev => {
                        let _ = app.emit_to(
                            label.as_str(),
                            "warden:cycle-tab",
                            serde_json::json!({ "label": label, "dir": -1 }),
                        );
                    }
                    TabNavAction::Next => {
                        let _ = app.emit_to(
                            label.as_str(),
                            "warden:cycle-tab",
                            serde_json::json!({ "label": label, "dir": 1 }),
                        );
                    }
                    TabNavAction::Jump(n) => {
                        let _ = app.emit_to(
                            label.as_str(),
                            "warden:select-tab",
                            serde_json::json!({ "label": label, "n": n }),
                        );
                    }
                }
                return;
            }
            if id == MENU_TAB_SPLIT {
                // ⌘D splits the active tab. The chrome owns "which tab is active", so it drives
                // the split_pane command on this event. Label-stamped + forMe()-filtered like
                // every other per-window emit (emit_to leaks to sibling webviews).
                let _ = app.emit_to(
                    label.as_str(),
                    MENU_TAB_SPLIT,
                    serde_json::json!({ "label": label }),
                );
            } else if id == shell_core::menu::ids::CLOSE_TAB {
                // ⌘W unloads the active tab (kill surface+PTY → cold, respawns on next focus),
                // it does NOT close the window. The chrome owns "which tab is active" + the
                // dot/highlight repaint, so it drives the unload_tab command on this event.
                let _ = app.emit_to(
                    label.as_str(),
                    "warden:unload-tab",
                    serde_json::json!({ "label": label }),
                );
            } else if id == shell_core::menu::ids::POP_OUT_TAB {
                // ⌘⇧O pops the active tab out into its own window. The chrome owns "which tab is
                // active", so it drives the pop_out_tab command on this event. Label-stamped +
                // forMe()-filtered like every per-window emit (emit_to leaks to siblings).
                let _ = app.emit_to(
                    label.as_str(),
                    "warden:pop-out-tab",
                    serde_json::json!({ "label": label }),
                );
            } else if id == shell_core::menu::ids::CHECK_UPDATES {
                // Manual update check → the focused window's chrome runs it (ignores auto_update).
                // Label-stamped like every other per-window emit: `emit_to` leaks to sibling
                // webviews, so without the stamp + the chrome's forMe() filter one menu click
                // would run an update check in every open window.
                let _ = app.emit_to(
                    label.as_str(),
                    "warden:check-update",
                    serde_json::json!({ "label": label }),
                );
            } else if id == shell_core::menu::ids::CLOSE_WINDOW {
                // ⌘⇧W closes the whole window (Destroyed → reap surfaces, then sync_empty_surface:
                // shows the home surface if it was the last real window — no quit).
                let _ = win.close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            set_hole_rect,
            init_tabs,
            activate_tab,
            unload_tab,
            split_pane,
            close_pane,
            focus_pane,
            close_hole,
            pop_out_tab,
            raise_popped_window,
            pop_in_tab,
            kill_session,
            start_session,
            rescan_root,
            shell_home_create_config,
            shell_home_edit_config,
            shell_home_open_window,
            probe_now
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;

                let handle = app.handle().clone();
                let mut mgr = WindowManager::new();
                // Load config; a config that parses (even with zero `[[window]]` entries) always
                // materializes — `materialize` builds nothing for an empty window list and
                // `sync_empty_surface` shows the home surface's window list (empty or not). A
                // missing/invalid config instead records the error and shows the home surface
                // directly: `sync_empty_surface`'s `home_state` call picks `NoConfig` when the file
                // doesn't exist, `Broken` when it does but didn't load. Recovery from either happens
                // in the watcher: the first valid load while no real window is live materializes.
                // Read the `notify_debug` toggle from the loaded config (default false) before the
                // config is consumed by materialize — it gates notify.rs's diagnostic trace.
                let mut notify_debug = false;
                match warden_config::load_with(&warden_config::config_path(), &login_shell()) {
                    Ok(loaded) => {
                        notify_debug = loaded.config.notify_debug;
                        mgr.materialize(&handle, loaded.config);
                    }
                    Err(e) => {
                        mgr.load_error = Some(e.to_string());
                        mgr.sync_empty_surface(&handle);
                    }
                }
                app.manage(ManagerState(std::sync::Mutex::new(mgr)));

                // Route terminal attention signals (bell / OSC 9/777 desktop notification) from
                // surfaces to their tabs (badge + macOS banner). Installs the surface-event sink;
                // needs ManagerState already managed (above) since the handler resolves surfaces
                // through it. `notify_debug` (config, default false) gates the diagnostic trace.
                notify::init(handle.clone(), notify_debug);

                // Background presence-probe scheduler — the single probe driver. Per-window
                // fast-until-stable bursts on key moments (tab activate, window focus/open, session
                // start/kill, hot-reload/rescan), decaying to the slow floor (`probe_interval`) or
                // Idle when it's 0. Triggers call `probe::bump`; nothing else spawns probe passes.
                {
                    let st = handle.state::<ManagerState>();
                    let interval = st.lock().probe_interval.clone();
                    let app_sched = handle.clone();
                    std::thread::spawn(move || probe::run_scheduler(app_sched, interval));
                }
                // No launch-time probe pass here: each window's chrome calls the
                // `probe_now` command once its `warden:session-state` listener is
                // registered (see init() in index.html), which populates the dots
                // reliably without racing the listener — covering `probe_interval = 0`
                // and background windows that never emit a launch `Focused`.

                // macOS menu. Windows are built at runtime with no NSMenu, so without this the
                // standard shortcuts are dead and there's nowhere to surface tab navigation.
                // Predefined items (Minimize/Quit) self-handle; custom items fire the Builder's
                // on_menu_event. Tab chords ⌘⇧[/⌘⇧] (prev/next) and the digit chords (⌘1–9
                // jump, or ⌘1/⌘2 cycle + ⌘3–9 jump under `tab_digit_keys = "cycle"`) are
                // macOS-standard and checked app-wide before any view, so they never collide
                // with the terminal. ⌘W unloads the active *tab* and ⌘⇧W closes the *window* — the Safari/
                // Chrome convention (close-tab vs close-window), NOT the predefined ⌘W=close-window.
                // The ⌘1/⌘2 chords depend on the config's `tab_digit_keys` mode
                // (read from last_good, set by the load above; default Jump when the
                // load failed and last_good is still the empty default). build_app_menu
                // rebuilds wholesale, so a hot-reload that flips the mode just calls it
                // again (see the watcher).
                rebuild_menu(app.handle())?;

                // Hot-reload: watch the config file; on each event reload + diff
                // against last_good + apply the resulting WindowOps to live
                // windows. The notify callback runs on a background thread, but
                // every Tauri/AppKit/registry touch is main-thread only — hop via
                // run_on_main_thread before doing any of it.
                let cfg_path = warden_config::config_path();
                // Watcher::with_default requires the config's parent dir to already exist.
                if let Some(parent) = cfg_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let wh = app.handle().clone();
                // The formatter's copy of the path (cfg_path is moved into the watcher).
                let fmt_path = cfg_path.clone();
                // Inject the login shell so hot-reload uses the same default as the initial load.
                let watcher =
                    warden_config::Watcher::with_default(cfg_path, login_shell(), move |res| {
                        let wh = wh.clone();
                        let fmt_path = fmt_path.clone();
                        let _ = wh.clone().run_on_main_thread(move || {
                            use tauri::{Emitter, Manager};
                            match res {
                                Ok(loaded) => {
                                    // Expand `[[window.root]]`s into the EFFECTIVE config
                                    // (recursive git-project scan) BEFORE taking the lock —
                                    // the walk is slow and must never run under the
                                    // ManagerState mutex, or it stalls the background probe
                                    // thread for its whole duration (mirrors probe.rs's
                                    // snapshot-then-release discipline). We're on the main
                                    // thread here, so no other writer interleaves. A config
                                    // that parses but declares zero `[[window]]` entries is
                                    // NOT special-cased here: `window_specs`/`reconcile` on an
                                    // empty windows list simply open nothing / close everything,
                                    // and `sync_empty_surface` below shows the home surface's
                                    // (now-empty) window list — not an error state.
                                    let new_eff = manager::effective_config(&loaded.config);
                                    let st = wh.state::<ManagerState>();
                                    let mut m = st.lock();
                                    // A previous save may have left an error recorded (Broken);
                                    // this load succeeded, so clear it before sync_empty_surface
                                    // reads it below. The `recover` branch's materialize_effective
                                    // also clears it, but the `else` reconcile branch doesn't call
                                    // that, so this is the one point both paths share.
                                    m.load_error = None;
                                    // The app menu is global, not part of window reconcile;
                                    // rebuilt below from current state.
                                    // Density + sidebar_drag are global too — a change to either
                                    // alone yields an empty reconcile (no per-window op), so nudge
                                    // every chrome below.
                                    let old_density = m.last_good.density;
                                    let new_density = loaded.config.density;
                                    let old_drag = m.last_good.sidebar_drag;
                                    let new_drag = loaded.config.sidebar_drag;
                                    // Recover (materialize, fresh-launch semantics that respect
                                    // open_on_start) ONLY when there is no baseline to reconcile
                                    // against — an empty `last_good` means we never had a valid
                                    // config. Reconciling from an empty baseline would emit Open for
                                    // EVERY window, since `reconcile` deliberately ignores
                                    // `open_on_start`, wrongly opening `open_on_start = false` ones.
                                    //
                                    // Do NOT also recover on "the home surface is showing an error":
                                    // the home-surface state can *become* Broken (close every
                                    // window, then save a half-written config — the Err branch below
                                    // records the error via `sync_empty_surface` regardless of live
                                    // windows), and recovering on the next good save would
                                    // re-materialize every window the user deliberately closed. An
                                    // empty `last_good` already covers every genuine no-baseline case
                                    // (invalid/missing config at launch, or a config later saved with
                                    // zero windows); a real baseline always has a non-empty
                                    // `last_good` (even a config with configured-but-closed windows),
                                    // so it correctly reconciles instead — unchanged config ⇒ windows
                                    // stay closed; an added window ⇒ opens.
                                    let recover = m.last_good.windows.is_empty();
                                    if recover {
                                        // Recovery: no reconcile baseline. Materialize from the
                                        // already-scanned effective config rather than reconciling
                                        // against an empty last_good.
                                        m.materialize_effective(
                                            &wh,
                                            loaded.config.clone(),
                                            new_eff,
                                        );
                                    } else {
                                        // Reconcile against the EFFECTIVE (root-expanded) configs
                                        // so a project appearing/vanishing on disk since last load
                                        // surfaces as a tab add/remove. Re-scans on every reload.
                                        let recon =
                                            warden_config::reconcile(&m.last_good, &new_eff);
                                        m.apply(&wh, &recon, &new_eff);
                                        // Advance the reconcile baseline ONLY on a valid load.
                                        m.last_good = new_eff;
                                        m.raw_config = loaded.config.clone();
                                        // A density/sidebar_drag flip alone produces no per-window
                                        // op, so apply() emitted nothing; re-push every window's
                                        // snapshot (now carrying the new globals) so each restyles.
                                        if old_density != new_density || old_drag != new_drag {
                                            m.refresh_all_chrome(&wh);
                                        }
                                    }
                                    // Update the empty-surface (home surface) now that the live
                                    // window set may have changed: recovery may have opened only
                                    // some (or zero, if all `open_on_start = false`) windows; a
                                    // reconcile may have closed the last one or opened the first.
                                    // Recomputes the window list fresh every call, so — unlike the
                                    // old launcher — no separate "push a stale list a refresh" step
                                    // is needed afterward: shell_core::home::show_home is idempotent
                                    // and this already re-shows/refreshes it with current entries.
                                    // (`recover`'s materialize_effective already calls this too;
                                    // idempotent, so the second call here is a harmless no-op then.)
                                    m.sync_empty_surface(&wh);
                                    // Apply the (possibly changed) probe cadence while we still
                                    // hold the lock, then release it before any lock-free work.
                                    m.set_probe_interval(loaded.config.probe_interval);
                                    drop(m);
                                    // Opt-in tidy: rewrite the file formatted. Diff-guarded in
                                    // format_file, so warden's own write doesn't loop the watcher.
                                    // Only runs on a clean parse (this branch).
                                    if loaded.config.format_on_save {
                                        let _ = warden_config::format_file(&fmt_path);
                                    }
                                    // Rebuild the global app menu: the digit-keys mode and/or the
                                    // window set may have changed (open/close ops in apply). Lock
                                    // was released at `drop(m)` above; rebuild_menu re-locks.
                                    let _ = rebuild_menu(&wh);
                                    // Clear any stale error banner.
                                    let _ = wh.emit("warden:error-clear", ());
                                    // Refresh the session dots now that cadence/config may have changed
                                    // (lock already released, so the bump can lock freely).
                                    probe::bump_all(&wh);
                                }
                                Err(e) => {
                                    // Keep last_good; record the error and let sync_empty_surface
                                    // route it — home_state's own precedence already no-ops when a
                                    // real window exists (has_windows wins over Broken), so it's safe
                                    // to call unconditionally. The banner is a separate concern (a
                                    // parse error mid-edit while windows are open needs its own
                                    // sidebar notice — sync_empty_surface has nothing to show there).
                                    let msg = e.to_string();
                                    let st = wh.state::<ManagerState>();
                                    let mut m = st.lock();
                                    m.load_error = Some(msg.clone());
                                    let had_windows = !m.is_empty();
                                    m.sync_empty_surface(&wh);
                                    drop(m);
                                    if had_windows {
                                        let _ = wh.emit("warden:error", msg);
                                    }
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
        .build(tauri::generate_context!())
        .expect("error while building warden")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                crate::manager::mark_quitting();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ratio_falls_back_and_clamps_to_the_draggable_band() {
        // The chrome's ratio is advisory and arrives over IPC, so every non-answer has to
        // land somewhere sane: a popped-out split must never open at a width the user could
        // not have dragged it to, and must never be laid out from a NaN.
        assert_eq!(split_ratio(None), DETACHED_DEFAULT_RATIO);
        assert_eq!(split_ratio(Some(f64::NAN)), DETACHED_DEFAULT_RATIO);
        assert_eq!(split_ratio(Some(f64::INFINITY)), DETACHED_DEFAULT_RATIO);
        assert_eq!(split_ratio(Some(0.3)), 0.3);
        assert_eq!(
            split_ratio(Some(0.0)),
            0.1,
            "clamped to the drag's own floor"
        );
        assert_eq!(
            split_ratio(Some(9.0)),
            0.9,
            "clamped to the drag's own ceil"
        );
        // The pair `pop_out_tab` builds `DetachSpec.panes` from must sum to the whole hole,
        // or the page lays the panes out at a width neither the ratio nor the window says.
        let r = split_ratio(Some(0.42));
        assert!((r + (1.0 - r) - 1.0).abs() < f64::EPSILON);
    }

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

    #[test]
    fn login_shell_uses_shell_env_with_login_flag() {
        std::env::set_var("SHELL", "/opt/homebrew/bin/fish");
        assert_eq!(login_shell(), "/opt/homebrew/bin/fish -l");
        // Empty/unset $SHELL falls back to the macOS default, still as a login shell.
        std::env::set_var("SHELL", "");
        assert_eq!(login_shell(), "/bin/zsh -l");
    }

    #[test]
    fn extract_sentinel_path_pulls_path_from_noisy_output() {
        // A login rc that prints a banner before and after the PATH readout must not corrupt it.
        let out = format!(
            "Welcome back!\n{PATH_SENTINEL_START}/opt/homebrew/bin:/usr/bin:/bin\n{PATH_SENTINEL_END}\nnvm: loaded\n"
        );
        assert_eq!(
            extract_sentinel_path(&out).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin:/bin")
        );
    }

    #[test]
    fn extract_sentinel_path_none_without_both_sentinels() {
        // Shell errored / printed nothing usable → leave PATH untouched.
        assert_eq!(extract_sentinel_path("command not found"), None);
        assert_eq!(
            extract_sentinel_path(&format!("{PATH_SENTINEL_START}/usr/bin")),
            None
        );
    }
}
