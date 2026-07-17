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
use manager::{InitDto, WindowManager, DIAG_LABEL};

use geometry::WebRect;

// Menu-item IDs, matched in the Builder's on_menu_event handler. The App/Config/Window submenus
// and Close Tab/Close Window are the shared spine now (`shell_core::menu`) — only warden's own
// items keep a local id. Direct-jump items use the prefix `tab_jump_<n>` (1-based position).
const MENU_TAB_PREV: &str = "tab_prev";
const MENU_TAB_NEXT: &str = "tab_next";
// ⌘1 / ⌘2 alias Next / Previous Tab. They live alongside ⌘⇧] / ⌘⇧[ and, by claiming
// the digit-1/2 chords, deliberately remove the digit-1/2 *jumps* — direct jumps start
// at ⌘3 (positions 1 and 2 have no direct chord under this layout).
const MENU_TAB_NEXT_DIGIT: &str = "tab_next_digit";
const MENU_TAB_PREV_DIGIT: &str = "tab_prev_digit";
const MENU_TAB_JUMP_PREFIX: &str = "tab_jump_";
// Reopen Last Closed (⌘⇧T) is warden-only — curator and lector have no equivalent, so this stays
// a local item spliced into the spine's Window submenu rather than added to the spine itself
// (YAGNI — add it there only if a sibling app wants it too).
const MENU_WINDOW_REOPEN_LAST: &str = "window_reopen_last";

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
/// item) interleaved with warden's own Tab submenu. The digit chords depend on `mode`:
/// - `Jump` (default): ⌘1–⌘9 jump straight to that 1-based tab position.
/// - `Cycle`: ⌘1 = next tab, ⌘2 = previous (distinct items firing the same
///   cycle-tab event — a menu item carries one accelerator), reclaiming the
///   digit-1/2 chords, so jumps shift to ⌘3–⌘9 (positions 1–2 lose their chord).
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
    use warden_config::TabDigitKeys;

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

    let prev = MenuItemBuilder::with_id(MENU_TAB_PREV, "Previous Tab")
        .accelerator("Shift+Cmd+BracketLeft")
        .build(app)?;
    let next = MenuItemBuilder::with_id(MENU_TAB_NEXT, "Next Tab")
        .accelerator("Shift+Cmd+BracketRight")
        .build(app)?;

    let mut tab_menu = SubmenuBuilder::new(app, "Tab").item(&prev).item(&next);

    // Cycle mode only: ⌘1/⌘2 as next/prev aliases. Kept alive past the `if` so the
    // builder's `&` items outlive the chained `.item()` calls below.
    let cycle_items = if mode == TabDigitKeys::Cycle {
        let next_digit = MenuItemBuilder::with_id(MENU_TAB_NEXT_DIGIT, "Next Tab (⌘1)")
            .accelerator("Cmd+Digit1")
            .build(app)?;
        let prev_digit = MenuItemBuilder::with_id(MENU_TAB_PREV_DIGIT, "Previous Tab (⌘2)")
            .accelerator("Cmd+Digit2")
            .build(app)?;
        Some((next_digit, prev_digit))
    } else {
        None
    };
    if let Some((next_digit, prev_digit)) = &cycle_items {
        tab_menu = tab_menu.item(next_digit).item(prev_digit);
    }

    // The spine's Close Tab (⌘W) — id/accelerator now live in shell_core::menu so they can't
    // drift per app; warden's own semantics (unload the active tab, NOT close the window) are
    // unchanged, see the on_menu_event handler's CLOSE_TAB arm.
    tab_menu = tab_menu.separator().item(&spine.close_tab).separator();

    // Jump-to-position. Jump mode: ⌘1–⌘9. Cycle mode: ⌘3–⌘9 (⌘1/⌘2 taken above).
    let first = if mode == TabDigitKeys::Cycle { 3 } else { 1 };
    let jumps = (first..=9)
        .map(|i| {
            MenuItemBuilder::with_id(format!("{MENU_TAB_JUMP_PREFIX}{i}"), format!("Tab {i}"))
                .accelerator(format!("Cmd+Digit{i}"))
                .build(app)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for j in &jumps {
        tab_menu = tab_menu.item(j);
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

/// Activate tab `id` within the calling window's registry. **Returns whether the tab is now live**
/// — i.e. whether a surface actually covers the content hole.
///
/// That return value is load-bearing, not informational. warden's window is transparent and the
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
) -> bool {
    use tauri::Emitter;
    let err = {
        let mut m = state.lock();
        m.windows
            .get_mut(window.label())
            .and_then(|ws| ws.registry.activate(&id).err())
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
    // A just-activated tab may have a session that changed while it was cold/backgrounded — fast-burst
    // this window so its dot is current within a pass rather than up to a slow-poll stale. Probe this
    // tab first so its dot settles immediately even in a wide window.
    probe::bump_tab(window.label(), &id);
    live
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
    // burst catches the absent→ transition the moment teardown lands. No optimistic drop and no
    // killing-suppression flag: the dot tracks `warden:session-state` and converges monotonically
    // (stays lit ~0.4–1s while `amux --kill` tears down, then drops once — no off→on→off flicker).
    // A genuinely-failed kill leaves the session present, so the dot correctly stays lit.
    std::thread::spawn(move || {
        let _ = probe::run_probe(&cmd, &dir);
        // Probe THIS tab first in the burst so its dot clears within one probe, not after every
        // other tab in a wide window's sweep.
        probe::bump_tab(&label, &id);
    });
}

/// Restart the *session* tab `id` represents by re-typing its startup `cmd` into the live shell
/// (see `Registry::start_session` — the runtime twin of spawn-time `initial_input`). No-op if the
/// tab is cold or has no `cmd`. Unlike `kill_session` (a single bump), the started session appears
/// **asynchronously** — the shell has to run the typed command — so a single burst can settle
/// "absent" before it comes up. Instead we bump now and again at ~1s/~3s so the scheduler's fast
/// burst re-arms and catches the session once it lands; the slow poll heals any later drift, and
/// `probe_interval = 0` tabs re-light on next focus. Same minimal-env PATH footgun as probes.
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
    let label = window.label().to_string();
    // The started session appears asynchronously — the shell has to run the typed `cmd` — and a
    // single burst can settle on "stably absent" before it comes up. So re-bump at ~1s and ~3s to
    // re-arm the scheduler's fast burst and catch a slow start; the slow poll heals any later drift,
    // and `probe_interval = 0` tabs light on the next event. (Delayed *bumps*, not reprobes — the
    // scheduler is the only prober.) This is the one trigger that needs more than a single bump.
    // Each bump names this tab so it's probed first in the burst (its dot lights as soon as the
    // session comes up, not after the rest of a wide window's sweep).
    probe::bump_tab(&label, &id);
    std::thread::spawn(move || {
        for delay_ms in [1000u64, 3000] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            probe::bump_tab(&label, &id);
        }
    });
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

    let mut m = state.lock();
    if let Some(ws) = m.windows.get_mut(window.label()) {
        ws.registry.set_active_frame(view_rect);
    }
}

/// Message the diagnostic window displays — read by `diagnostic.html` on load.
#[cfg(target_os = "macos")]
#[tauri::command]
fn diagnostic_message(state: tauri::State<ManagerState>) -> String {
    state.lock().diagnostic_msg.clone()
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

/// The window-state plugin's save file, namespaced to the **resolved config path** so two
/// configs that name a window the same don't share saved bounds. Without this, the test/example
/// config (`just run` → `examples/config.toml`) and a prod `~/.config/warden/config.toml` both
/// key window state by `sanitize_label(title)` in the *same* file — identical titles collide and
/// the test window restores prod's size/position. Bounds belong to a (config, window) pair, so we
/// scope the filename by a stable hash of the config path (canonicalized when it exists, so a
/// symlinked path doesn't fork the state). The hash is `fnv1a_64` — a fixed, toolchain-independent
/// algorithm — deliberately NOT `std`'s `DefaultHasher`, whose output isn't guaranteed stable
/// across Rust releases: a `rust-toolchain.toml` bump could silently change the filename and reset
/// every window to default bounds. Moving/renaming the config orphans its saved bounds —
/// acceptable, since the path is otherwise stable (`config_path()`).
fn window_state_filename() -> String {
    let path = warden_config::config_path();
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    let hash = fnv1a_64(canonical.as_os_str().as_encoded_bytes());
    format!(".window-state-{hash:016x}.json")
}

/// FNV-1a 64-bit hash. Small, deterministic, and — crucially — **stable across Rust toolchains**
/// (unlike `std::hash::DefaultHasher`), so the value drives a persistent on-disk filename without
/// risk of a toolchain bump changing it. Non-cryptographic; collision resistance is irrelevant
/// here (the input is a single trusted config path).
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
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

    // Register the shell-core plugins (window-state + updater + process) — the set every sibling
    // app installs identically. Window-state persists each window's size/position/maximized keyed by
    // Tauri label *within a per-config state file* (window_state_filename) so two configs sharing a
    // window title don't share bounds; restore is explicit in manager.rs::build_window (runtime-built
    // windows). The transient diagnostic + shared home surface windows are excluded from state
    // restore.
    shell_core::register_plugins(
        tauri::Builder::default(),
        window_state_filename(),
        &[DIAG_LABEL, shell_core::home::HOME_LABEL],
    )
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
        if id == MENU_TAB_PREV || id == MENU_TAB_PREV_DIGIT {
            let _ = app.emit_to(
                label.as_str(),
                "warden:cycle-tab",
                serde_json::json!({ "label": label, "dir": -1 }),
            );
        } else if id == MENU_TAB_NEXT || id == MENU_TAB_NEXT_DIGIT {
            let _ = app.emit_to(
                label.as_str(),
                "warden:cycle-tab",
                serde_json::json!({ "label": label, "dir": 1 }),
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
        } else if let Some(n) = id
            .strip_prefix(MENU_TAB_JUMP_PREFIX)
            .and_then(|s| s.parse::<u32>().ok())
        {
            let _ = app.emit_to(
                label.as_str(),
                "warden:select-tab",
                serde_json::json!({ "label": label, "n": n }),
            );
        }
    })
    .invoke_handler(tauri::generate_handler![
        set_hole_rect,
        init_tabs,
        activate_tab,
        unload_tab,
        kill_session,
        start_session,
        rescan_root,
        diagnostic_message,
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
            // Load config; on a missing/invalid/empty config, fall back to a
            // single diagnostic window instead of materializing windows.
            // Recovery happens in the watcher: the first valid load while no
            // window window is live materializes + closes the diagnostic.
            // Read the `notify_debug` toggle from the loaded config (default false) before the
            // config is consumed by materialize — it gates notify.rs's diagnostic trace.
            let mut notify_debug = false;
            match warden_config::load_with(&warden_config::config_path(), &login_shell()) {
                Ok(loaded) if !loaded.config.windows.is_empty() => {
                    notify_debug = loaded.config.notify_debug;
                    mgr.materialize(&handle, loaded.config);
                }
                Ok(loaded) => {
                    notify_debug = loaded.config.notify_debug;
                    mgr.show_diagnostic(&handle, "config has no [[window]] entries");
                }
                Err(e) => mgr.show_diagnostic(&handle, &e.to_string()),
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
            // (read from last_good, set by the load above; default Jump for the
            // diagnostic-at-launch case). build_app_menu rebuilds wholesale, so a
            // hot-reload that flips the mode just calls it again (see the watcher).
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
                            Ok(loaded) if !loaded.config.windows.is_empty() => {
                                // Expand `[[window.root]]`s into the EFFECTIVE config
                                // (recursive git-project scan) BEFORE taking the lock —
                                // the walk is slow and must never run under the
                                // ManagerState mutex, or it stalls the background probe
                                // thread for its whole duration (mirrors probe.rs's
                                // snapshot-then-release discipline). We're on the main
                                // thread here, so no other writer interleaves.
                                let new_eff = manager::effective_config(&loaded.config);
                                let st = wh.state::<ManagerState>();
                                let mut m = st.lock();
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
                                // Do NOT also recover on "the diagnostic is up": the home-surface
                                // state can *become* the diagnostic state (close every window,
                                // then save a half-written config — the Err branch below routes
                                // to the diagnostic because `is_empty()`), and recovering on the
                                // next good save would re-materialize every window the user
                                // deliberately closed. An empty `last_good` already covers every
                                // genuine diagnostic case (invalid/missing config at launch, or a
                                // manually-closed diagnostic); the home-surface state always has a
                                // non-empty `last_good`, so it correctly reconciles instead —
                                // unchanged config ⇒ windows stay closed; an added window ⇒ opens.
                                // `sync_empty_surface` below clears the diagnostic either way.
                                let recover = m.last_good.windows.is_empty();
                                if recover {
                                    // Recovery: no reconcile baseline (diagnostic state, or a
                                    // manually-closed diagnostic). Materialize from the
                                    // already-scanned effective config and close the
                                    // diagnostic, rather than reconciling against an
                                    // empty last_good.
                                    m.materialize_effective(&wh, loaded.config.clone(), new_eff);
                                    m.clear_diagnostic(&wh);
                                } else {
                                    // Reconcile against the EFFECTIVE (root-expanded) configs
                                    // so a project appearing/vanishing on disk since last load
                                    // surfaces as a tab add/remove. Re-scans on every reload.
                                    let recon = warden_config::reconcile(&m.last_good, &new_eff);
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
                                // Update the empty-surface (home surface/diagnostic) now that
                                // the live window set may have changed: recovery may have opened
                                // only some (or zero, if all `open_on_start = false`) windows; a
                                // reconcile may have closed the last one or opened the first.
                                // Recomputes the window list fresh every call, so — unlike the
                                // old launcher — no separate "push a stale list a refresh" step
                                // is needed afterward: shell_core::home::show_home is idempotent
                                // and this already re-shows/refreshes it with current entries.
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
                            Ok(_) => {
                                // Valid TOML but no windows. If live windows exist,
                                // keep them up and surface the error banner; if we're
                                // already in the diagnostic state (no live windows),
                                // there's no banner — route it to the diagnostic so a
                                // second bad save doesn't leave the launch text stale.
                                let msg = "config has no [[window]] entries".to_string();
                                let st = wh.state::<ManagerState>();
                                let mut m = st.lock();
                                if m.is_empty() {
                                    shell_core::home::close_home(&wh);
                                    m.show_diagnostic(&wh, &msg);
                                } else {
                                    drop(m);
                                    let _ = wh.emit("warden:error", msg);
                                }
                            }
                            Err(e) => {
                                // Keep last_good; surface the parse error in the banner,
                                // or in the diagnostic when no live window can show one.
                                let msg = e.to_string();
                                let st = wh.state::<ManagerState>();
                                let mut m = st.lock();
                                if m.is_empty() {
                                    shell_core::home::close_home(&wh);
                                    m.show_diagnostic(&wh, &msg);
                                } else {
                                    drop(m);
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
    fn fnv1a_64_matches_known_vectors() {
        // Canonical FNV-1a/64 test vectors — pins the algorithm so the window-state filename can
        // never drift with the toolchain (the whole point of not using DefaultHasher).
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn window_state_filename_shape_is_stable() {
        // Same config path → same filename, every run (no per-run seed).
        assert_eq!(window_state_filename(), window_state_filename());
        assert!(window_state_filename().starts_with(".window-state-"));
        assert!(window_state_filename().ends_with(".json"));
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
