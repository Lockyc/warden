//! Routes terminal attention signals (bell / desktop notification) from a surface to its tab.
//!
//! The surface seam (`surface::set_surface_event_sink`) hands us a seam-neutral `SurfaceEvent`
//! whenever libghostty reports `RING_BELL` or a `DESKTOP_NOTIFICATION` (OSC 9 / OSC 777). We map
//! the surface back to its (window, tab) via the `WindowManager`, then badge the tab in that
//! window's chrome (`warden:notify`) and — for a desktop notification — raise a macOS banner. Both
//! happen only when the tab is **not** already visible (focused window + active tab): if you're
//! looking at it, there's nothing to notify. This is what replaces agentmux's direct `osascript`
//! call — the signal now flows through the terminal and lands on the right tab.
//!
//! The banner is shell-core's `banner`, stamped with its originating `(window label, tab id)` and,
//! for an agent tab, the agent as its `context`; a click surfaces the terminal that raised it
//! (`focus_window_tab`), even from the background.

use crate::surface::{SurfaceEvent, SurfaceSignal};
use crate::ManagerState;
use shell_core::banner::{self, Banner, Click};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

/// Diagnostic toggle (config `notify_debug`, default false; read once at launch). When on, the
/// notification path is traced to a log file. Exists to investigate the macOS-26 banner-suppression
/// issue where warden posts an accepted, `willPresent`-handled `.banner` that the OS never renders
/// (see docs/notifications.md): the trace proves warden's side is correct so the suspect is the OS.
static NOTIFY_DEBUG: AtomicBool = AtomicBool::new(false);

/// Where the `notify_debug` trace is written: under the per-user temp dir (`$TMPDIR`, per-user on
/// macOS) rather than a world-shared `/tmp` path, so another local user can't read the traced
/// notification text or pre-plant a symlink to redirect our appends into a file they own. A fresh
/// log per boot is exactly what the reboot-and-retry workflow wants.
fn notify_debug_log() -> std::path::PathBuf {
    std::env::temp_dir().join("warden-notify-dbg.log")
}

/// Append a line to the trace log — a no-op unless `notify_debug` is enabled. Best-effort: a failed
/// open is silently ignored (diagnostics must never affect the notification path). Also the sink
/// for shell-core's own banner steps (`banner::set_trace`).
fn dbglog(msg: &str) {
    if !NOTIFY_DEBUG.load(Ordering::Relaxed) {
        return;
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt as _;
    // O_NOFOLLOW (macOS value) refuses to open through a symlink planted at the path; 0600 keeps the
    // trace unreadable to other local users. warden-app is macOS-only, so the literal flag is safe.
    const O_NOFOLLOW: i32 = 0x0100;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(notify_debug_log())
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// A clicked banner: raise the window owning it and ask its chrome to select the tab. Best-effort:
/// an unknown label (window since closed) raises nothing; a stale/missing tab id leaves the active
/// tab alone, so the click still lands you in the right window. A tab popped out of `label` is
/// shown by its detached window, so that window is the one raised, and the origin's selection is
/// left alone (selecting a popped-out row only raises that same window). `emit_to` leaks to sibling
/// webviews (see CLAUDE.md), so the payload carries the target `label` and the chrome drops events
/// meant for another window — same guard as every other per-window event. A banner naming an agent
/// tab (its `context`) selects it first, as the user's own pick, so the tab opens on it.
fn focus_window_tab(app: &AppHandle, click: &Click) {
    focus_tab(app, &click.window, &click.tab, click.context.as_deref());
}

/// Raise window `label` on tab `id`, selecting agent tab `agent` first when given — the
/// [`focus_window_tab`] path. Its chrome must already be listening: one that hasn't mounted
/// takes the tab through `WindowState::pending_focus` instead.
pub fn focus_tab(app: &AppHandle, label: &str, id: &str, agent: Option<&str>) {
    let popped = app
        .state::<ManagerState>()
        .lock()
        .detached_label_for(label, id);
    if let Some(dlabel) = popped {
        banner::raise(app, &dlabel);
        return;
    }
    banner::raise(app, label);
    if let Some(agent) = agent {
        if let Err(e) = crate::rex::lifecycle::select(app, label, id, agent) {
            log::error!("{label}: {id}: {e}");
        }
    }
    let _ = app.emit_to(
        label,
        "warden:focus-tab",
        serde_json::json!({ "label": label, "id": id }),
    );
}

/// Install the surface-signal handler and prime native banners. Called once at setup (on the main
/// thread). `debug` is the config `notify_debug` flag, read once here (a hot-reload change needs a
/// restart).
pub fn init(app: AppHandle, debug: bool) {
    NOTIFY_DEBUG.store(debug, Ordering::Relaxed);
    if debug {
        banner::set_trace(dbglog);
    }
    banner::init(&app, focus_window_tab);
    // The sink is the single entry point for every surface signal, so it fans out by kind: a dead
    // child is a *tab-state* change (unload to cold), not an attention signal, so it goes to the
    // manager rather than through the badge/banner path below.
    crate::surface::set_surface_event_sink(move |event| match event.signal {
        SurfaceSignal::ChildExited { .. } => {
            crate::manager::WindowManager::handle_child_exited(&app, event.surface_id);
            crate::sync_agent_menu(&app); // the active tab may have changed
        }
        // Likewise a focus change is tab bookkeeping (which pane types), not attention.
        SurfaceSignal::Focused => {
            crate::manager::WindowManager::handle_surface_focused(&app, event.surface_id);
            crate::sync_agent_menu(&app); // the agent-step and pane items follow the focused pane
        }
        // Likewise the pointer crossing onto a surface is chrome bookkeeping (the sidebar's hover
        // state), not attention — and it fires on every boundary crossing, so it must never reach
        // the badge/banner path below.
        SurfaceSignal::PointerEntered => {
            crate::manager::WindowManager::handle_pointer_entered(&app, event.surface_id)
        }
        // A Rex pane's size claim, raised on every press: never attention.
        SurfaceSignal::UserInput => {
            crate::manager::WindowManager::handle_user_input(&app, event.surface_id)
        }
        // A request for warden's menu: never attention.
        SurfaceSignal::ContextMenu { x, y } => {
            crate::manager::WindowManager::handle_context_menu(&app, event.surface_id, x, y)
        }
        _ => handle(&app, event),
    });
    if debug {
        dbglog("--- notify_debug enabled; tracing notification path ---");
    }
}

/// Runs on the main thread, in its own main-queue turn: `action_cb` hands bell, notification and
/// child-exit to the sink through `defer_event`, never synchronously. So this never nests inside
/// a command that already holds `ManagerState`. Locking `ManagerState` and touching Tauri/AppKit here is therefore safe and deadlock-free.
/// Uses `ManagerState::lock` (not a bare `unwrap`) so a poisoned mutex recovers instead of
/// crashing the notification path — matching every command handler.
fn handle(app: &AppHandle, event: SurfaceEvent) {
    if NOTIFY_DEBUG.load(Ordering::Relaxed) {
        let kind = match &event.signal {
            SurfaceSignal::Bell => "Bell (badge only, no banner by design)".to_string(),
            SurfaceSignal::Notification { title, body } => {
                format!("Notification(title={title:?}, body={body:?})")
            }
            // Routed to the manager before reaching here (see the sink in `init`).
            SurfaceSignal::ChildExited { exit_code } => format!("ChildExited({exit_code:?})"),
            SurfaceSignal::Focused => "Focused (routed to the manager)".to_string(),
            SurfaceSignal::PointerEntered => "PointerEntered (routed to the manager)".to_string(),
            SurfaceSignal::UserInput => "UserInput (routed to the manager)".to_string(),
            SurfaceSignal::ContextMenu { .. } => "ContextMenu (routed to the manager)".to_string(),
        };
        dbglog(&format!(
            "handle: signal={kind} surface={}",
            event.surface_id
        ));
    }
    let located = locate(app, event.surface_id);
    let Some((label, tab, visible)) = located else {
        dbglog("handle: surface not located (unloaded?) -> dropped");
        return; // surface not found (e.g. just unloaded) — drop the signal
    };
    // A desktop notification (OSC 9/777) additionally raises a macOS banner; a bare bell only
    // badges (no text to show, and bells are frequent enough that banners would be noise).
    let banner = match event.signal {
        SurfaceSignal::Notification { title, body } if title.trim().is_empty() => {
            Some(("warden".to_string(), body))
        }
        SurfaceSignal::Notification { title, body } => Some((title, body)),
        _ => None,
    };
    raise(app, &label, &tab, visible, banner, None, false);
}

/// Raise attention on tab `tab` of window `label` unless it's `visible`: badge its sidebar row —
/// a padlock rather than the dot when `lock` (an agent waiting on a permission) — and, with a
/// `banner` (title, body), post one. A banner for an `agent` tab stamps it, so a click selects
/// that agent tab too. Main thread only. Both a surface's signals ([`handle`]) and a hidden agent
/// tab needing you (`rex::lifecycle::attend`) land here.
pub fn raise(
    app: &AppHandle,
    label: &str,
    tab: &str,
    visible: bool,
    banner: Option<(String, String)>,
    agent: Option<&str>,
    lock: bool,
) {
    if visible {
        dbglog(&format!(
            "raise: tab {label}/{tab} is visible -> no notify (correct)"
        ));
        return; // the user is already looking at this tab
    }

    // Badge the tab in its window's sidebar; the chrome marks it unread until focused. emit_to
    // leaks to sibling windows here (see CLAUDE.md), so stamp the target window label into the
    // payload and let the chrome drop events meant for a sibling — same guard as every other
    // per-window event. Without it, a hidden tab's bell badges a same-titled tab in another window.
    let _ = app.emit_to(
        label,
        "warden:notify",
        serde_json::json!({ "label": label, "id": tab, "lock": lock }),
    );

    if let Some((title, body)) = banner {
        banner::fire(&Banner {
            title: &title,
            body: &body,
            window: label,
            tab,
            context: agent,
        });
    }
}

/// The `(window label, tab id, visible)` a signal from `surface_id` belongs to. A docked surface is
/// `WindowManager::locate_surface`'s answer; a popped-out one belongs to its tab's row in the ORIGIN
/// window (the row a badge lands on and a banner names), and is visible while its detached window
/// is focused — a popped-out tab is the only thing that window shows.
fn locate(app: &AppHandle, surface_id: usize) -> Option<(String, String, bool)> {
    let state = app.state::<ManagerState>();
    let m = state.lock();
    if let Some(found) = m.locate_surface(surface_id) {
        return Some(found);
    }
    let (dlabel, _) = m.locate_detached_surface(surface_id)?;
    let ds = m.detached.get(&dlabel)?;
    let visible = app
        .get_window(&dlabel)
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false);
    Some((ds.origin_label.clone(), ds.tab_id.clone(), visible))
}
