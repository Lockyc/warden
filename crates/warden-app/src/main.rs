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
mod notify;

#[cfg(target_os = "macos")]
mod probe;

#[cfg(target_os = "macos")]
mod registry;

#[cfg(target_os = "macos")]
use manager::{InitDto, WindowManager, DIAG_LABEL};

use geometry::WebRect;

// Menu-item IDs, matched in the Builder's on_menu_event handler.
// Direct-jump items use the prefix `tab_jump_<n>` (1-based position).
const MENU_TAB_PREV: &str = "tab_prev";
const MENU_TAB_NEXT: &str = "tab_next";
// ⌘1 / ⌘2 alias Next / Previous Tab. They live alongside ⌘⇧] / ⌘⇧[ and, by claiming
// the digit-1/2 chords, deliberately remove the digit-1/2 *jumps* — direct jumps start
// at ⌘3 (positions 1 and 2 have no direct chord under this layout).
const MENU_TAB_NEXT_DIGIT: &str = "tab_next_digit";
const MENU_TAB_PREV_DIGIT: &str = "tab_prev_digit";
const MENU_TAB_CLOSE: &str = "tab_close";
const MENU_TAB_JUMP_PREFIX: &str = "tab_jump_";
const MENU_WINDOW_CLOSE: &str = "window_close";
const MENU_WINDOW_REOPEN_LAST: &str = "window_reopen_last";
// Per-window items: id = this prefix + the window's Tauri label.
const MENU_WINDOW_PREFIX: &str = "window_open_";
// Config menu: open the config file in the default editor / reveal it in Finder.
const MENU_CONFIG_EDIT: &str = "config_edit";
const MENU_CONFIG_REVEAL: &str = "config_reveal";

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

/// Build and install the app menu. The digit chords depend on `mode`:
/// - `Jump` (default): ⌘1–⌘9 jump straight to that 1-based tab position.
/// - `Cycle`: ⌘1 = next tab, ⌘2 = previous (distinct items firing the same
///   cycle-tab event — a menu item carries one accelerator), reclaiming the
///   digit-1/2 chords, so jumps shift to ⌘3–⌘9 (positions 1–2 lose their chord).
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
    use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
    use warden_config::TabDigitKeys;

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

    tab_menu = tab_menu.separator().item(&close_tab).separator();

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

    // Window menu: reopen-last (⌘⇧T) + one row per configured window. Open windows
    // get a checkmark and raise on select; closed windows show "(closed)" and reopen.
    let reopen_last = MenuItemBuilder::with_id(MENU_WINDOW_REOPEN_LAST, "Reopen Last Closed")
        .accelerator("Shift+Cmd+KeyT")
        .enabled(reopen_available)
        .build(app)?;
    let mut window_menu = SubmenuBuilder::new(app, "Window")
        .item(&reopen_last)
        .separator();
    // Build the per-window check items first so their `&` refs outlive the chained
    // `.item()` calls (same pattern as the tab jumps above).
    let window_items = entries
        .iter()
        .map(|e| {
            let text = if e.open {
                e.title.clone()
            } else {
                format!("{}  (closed)", e.title)
            };
            CheckMenuItemBuilder::with_id(format!("{MENU_WINDOW_PREFIX}{}", e.label), text)
                .checked(e.open)
                .build(app)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for it in &window_items {
        window_menu = window_menu.item(it);
    }
    let window_menu = window_menu.build()?;

    // Config menu: open the config file in the default editor, or reveal it in Finder — so the
    // user needn't memorise ~/.config/warden/config.toml. No accelerators (matches curator's
    // Config submenu); the items act on the file, not a window (routed in on_menu_event).
    let edit_cfg = MenuItemBuilder::with_id(MENU_CONFIG_EDIT, "Edit Config").build(app)?;
    let reveal_cfg =
        MenuItemBuilder::with_id(MENU_CONFIG_REVEAL, "Reveal Config in Finder").build(app)?;
    let config_menu = SubmenuBuilder::new(app, "Config")
        .item(&edit_cfg)
        .item(&reveal_cfg)
        .build()?;

    let menu = MenuBuilder::new(app)
        .item(&app_menu)
        .item(&tab_menu)
        .item(&config_menu)
        .item(&window_menu)
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
fn probe_now(window: tauri::WebviewWindow) {
    use tauri::Manager;
    probe::spawn_pass(
        window.app_handle().clone(),
        Some(window.label().to_string()),
    );
}

/// Activate tab `id` within the calling window's registry.
#[cfg(target_os = "macos")]
#[tauri::command]
fn activate_tab(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    use tauri::Emitter;
    let err = {
        let mut m = state.lock();
        m.windows
            .get_mut(window.label())
            .and_then(|ws| ws.registry.activate(&id).err())
    };
    // A lazy spawn failed on click: the tab stays cold (blank placeholder) instead
    // of panicking. The chrome is listening now, so push the reason to the banner.
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
/// live. No-op if the tab has no `kill` set. After the kill completes, re-probe this window
/// so the cyan presence dot drops once the session is actually gone (the poll loop would
/// also re-converge, but this makes it prompt). Same minimal-env PATH footgun as probes —
/// see scrub note + CLAUDE.md.
#[cfg(target_os = "macos")]
#[tauri::command]
fn kill_session(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    use tauri::Manager;
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
    let app = window.app_handle().clone();
    let label = window.label().to_string();
    // Run the kill, then re-probe THIS window on the same thread so the order is
    // deterministic: the cyan presence dot drops only after the session is actually
    // gone. Off the UI thread (kill + probe are slow `sh -c` calls); the exit code of
    // the kill is ignored (fire-and-forget — warden has no response to a failed kill).
    // Sequencing (not racing a separate spawn_pass) matters because with
    // `probe_interval = 0` there's no timer to heal a re-probe that ran before the kill.
    std::thread::spawn(move || {
        let _ = probe::run_probe(&cmd, &dir);
        probe::run_pass(&app, Some(&label));
    });
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
/// symlinked path doesn't fork the state). Deterministic across runs: `DefaultHasher::new()` uses
/// fixed seeds. Moving/renaming the config orphans its saved bounds — acceptable, since the path
/// is otherwise stable (`config_path()`).
fn window_state_filename() -> String {
    use std::hash::{Hash, Hasher};
    let path = warden_config::config_path();
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut h);
    format!(".window-state-{:016x}.json", h.finish())
}

fn main() {
    // warden hosts terminals — it must not leak its own launcher's tmux membership into them
    // (breaks nested agentmux/tmux). Scrub before anything else inherits the environment.
    scrub_inherited_tmux_env();
    // A Dock/Finder/Spotlight launch gives warden only the minimal launchd PATH, so a config
    // `shell`/`probe`/`kill` named by bare command would be not-found. Adopt the login-shell PATH
    // before any surface or probe spawns and inherits this process's environment.
    restore_login_path();

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
        // Persist each window's size + position (+ maximized) across launches, keyed by
        // Tauri label *within a per-config state file* (see window_state_filename) so two configs
        // that share a window title don't share bounds. Saving is automatic (on close/exit);
        // restore is triggered explicitly in manager.rs::build_window since warden's windows are
        // built at runtime, not from tauri.conf.json. The transient diagnostic window is excluded
        // — its bounds are throwaway and must not bleed into a real window that reuses nothing.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .skip_initial_state(DIAG_LABEL)
                .with_filename(window_state_filename())
                .build(),
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

            // Window menu acts on the manager/app, not the focused window — handle it
            // before the focused-window lookup (reopen-last needs no focused window).
            if id == MENU_WINDOW_REOPEN_LAST {
                let st = app.state::<ManagerState>();
                let reopened = { st.lock().reopen_last(app) };
                if reopened {
                    let _ = rebuild_menu(app);
                }
                return;
            }
            if let Some(win_label) = id.strip_prefix(MENU_WINDOW_PREFIX) {
                let st = app.state::<ManagerState>();
                {
                    let mut m = st.lock();
                    if m.windows.contains_key(win_label) {
                        m.focus_window(win_label);
                    } else {
                        m.reopen_window(app, win_label);
                    }
                }
                let _ = rebuild_menu(app);
                return;
            }

            // Config menu acts on the config file, not a window — handle before the focused-window
            // lookup (no window need be focused). `open` routes to the default editor; `open -R`
            // reveals in Finder. config_path() is WARDEN_CONFIG else ~/.config/warden/config.toml.
            if id == MENU_CONFIG_EDIT {
                let _ = std::process::Command::new("open")
                    .arg(warden_config::config_path())
                    .spawn();
                return;
            }
            if id == MENU_CONFIG_REVEAL {
                let _ = std::process::Command::new("open")
                    .arg("-R")
                    .arg(warden_config::config_path())
                    .spawn();
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
            } else if id == MENU_TAB_CLOSE {
                // ⌘W unloads the active tab (kill surface+PTY → cold, respawns on next focus),
                // it does NOT close the window. The chrome owns "which tab is active" + the
                // dot/highlight repaint, so it drives the unload_tab command on this event.
                let _ = app.emit_to(
                    label.as_str(),
                    "warden:unload-tab",
                    serde_json::json!({ "label": label }),
                );
            } else if id == MENU_WINDOW_CLOSE {
                // ⌘⇧W closes the whole window window (Destroyed → reap surfaces, last-window-quit).
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
            diagnostic_message,
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

                // Background session-probe poll loop. Reads the shared interval each
                // tick so a hot-reload can change cadence (0 = focus/refresh-only).
                {
                    use std::sync::atomic::Ordering;
                    use std::time::Duration;
                    // Wait in ≤3s slices, bailing the moment the interval changes, so a
                    // hot-reload that shortens a long cadence (e.g. 60→5) or toggles the
                    // timer on/off is picked up within a few seconds — not after the old
                    // sleep elapses. This is what makes the cadence genuinely live.
                    const SLICE: u64 = 3;
                    let st = handle.state::<ManagerState>();
                    let interval = st.lock().probe_interval.clone();
                    let app_poll = handle.clone();
                    std::thread::spawn(move || loop {
                        let secs = interval.load(Ordering::Relaxed);
                        if secs > 0 {
                            probe::run_pass(&app_poll, None);
                        }
                        // secs == 0 → idle; still slice-sleep so re-enabling is responsive.
                        let target = if secs > 0 { secs } else { SLICE };
                        let mut slept = 0;
                        while slept < target && interval.load(Ordering::Relaxed) == secs {
                            let chunk = std::cmp::min(SLICE, target - slept);
                            std::thread::sleep(Duration::from_secs(chunk));
                            slept += chunk;
                        }
                    });
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
                                    let st = wh.state::<ManagerState>();
                                    let mut m = st.lock();
                                    // The app menu is global, not part of window reconcile;
                                    // rebuilt below from current state.
                                    // Density is global too — a density-only edit yields an empty
                                    // reconcile (no per-window op), so nudge every chrome below.
                                    let old_density = m.last_good.density;
                                    let new_density = loaded.config.density;
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
                                        m.apply(&wh, &recon, loaded.config.density.as_str());
                                        // Advance the reconcile baseline ONLY on a valid load.
                                        m.last_good = loaded.config.clone();
                                        // A density flip alone produces no per-window op, so
                                        // apply() emitted nothing; re-push every window's snapshot
                                        // (now carrying the new density) so each chrome restyles.
                                        if old_density != new_density {
                                            m.refresh_all_chrome(&wh);
                                        }
                                    }
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
                                    // (lock already released, so the spawned pass can lock freely).
                                    probe::spawn_pass(wh.clone(), None);
                                }
                                Ok(_) => {
                                    // Valid TOML but no windows: keep live windows up,
                                    // surface the error banner rather than tearing down.
                                    let _ = wh.emit(
                                        "warden:error",
                                        "config has no [[window]] entries".to_string(),
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
