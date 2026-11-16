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
/// tab alone, so the click still lands you in the right window. A tab popped out of `label` since
/// the banner fired is shown by its pop-out, so that window is the one raised
/// ([`attention_target`]). `emit_to` leaks to sibling
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
    let label = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        let windows = m
            .windows
            .iter()
            .map(|(l, ws)| (l.as_str(), ws.origin.as_ref(), &ws.registry));
        attention_target(label, id, windows).to_owned()
    };
    let label = label.as_str();
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
    let Some(at) = located else {
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
    raise(app, &at, banner);
}

/// Raise a surface's attention on the tab `at` names unless it's visible: badge its sidebar rows
/// ([`tab_rows`](crate::manager::tab_rows)) and, with a `banner` (title, body), post one. Main
/// thread only. An agent tab's need badges its row from the agent list instead, and banners
/// through [`agent_banner`].
fn raise(app: &AppHandle, at: &Located, banner: Option<(String, String)>) {
    let (label, tab) = (at.label.as_str(), at.tab.as_str());
    if at.visible {
        dbglog(&format!(
            "raise: tab {label}/{tab} is visible -> no notify (correct)"
        ));
        return; // the user is already looking at this tab
    }

    app.state::<ManagerState>()
        .lock()
        .set_unread(label, tab, true);
    // Badge the tab's rows; the chrome marks each unread until seen. emit_to leaks to sibling
    // windows here (see CLAUDE.md), so stamp the target window label into the payload and let the
    // chrome drop events meant for a sibling — same guard as every other per-window event.
    // Without it, a hidden tab's bell badges a same-titled tab in another window.
    for (row_label, row_tab) in crate::manager::tab_rows(label, tab, at.origin.as_ref()) {
        let _ = app.emit_to(
            row_label,
            "warden:notify",
            serde_json::json!({ "label": row_label, "id": row_tab }),
        );
    }

    if let Some((title, body)) = banner {
        banner::fire(&Banner {
            title: &title,
            body: &body,
            window: label,
            tab,
            context: None,
        });
    }
}

/// Post a banner for agent tab `agent` of tab `tab` in window `label`; a click selects it.
pub fn agent_banner(label: &str, tab: &str, title: &str, body: &str, agent: &str) {
    banner::fire(&Banner {
        title,
        body,
        window: label,
        tab,
        context: Some(agent),
    });
}

/// Where a signal from a surface lands ([`locate`]).
struct Located {
    /// The window whose registry holds the surface — a pop-out's for a popped-out tab.
    label: String,
    tab: String,
    /// Whether that window is focused with the surface on screen.
    visible: bool,
    /// The pop-out's origin, whose placeholder row badges too.
    origin: Option<crate::manager::PopOrigin>,
}

fn locate(app: &AppHandle, surface_id: usize) -> Option<Located> {
    let (label, tab, shown, origin) = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        let (label, tab, shown) = m.locate_surface(surface_id)?;
        let origin = m.windows.get(&label).and_then(|ws| ws.origin.clone());
        (label, tab, shown, origin)
    };
    let focused = app
        .get_window(&label)
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false);
    Some(Located {
        label,
        tab,
        visible: shown && focused,
        origin,
    })
}

/// The window a banner for tab `tab` of window `label` raises — wherever the tab is when it is
/// clicked: `label` while its registry holds the tab; else the pop-out the tab went out to from
/// `label`; else any window holding it now (a pop-out's tab gone home, the pop-out with it).
/// `label` itself when no window holds the tab. `windows` is every open window as `(label, origin
/// when a pop-out, registry)`.
pub(crate) fn attention_target<'a>(
    label: &'a str,
    tab: &str,
    windows: impl IntoIterator<
        Item = (
            &'a str,
            Option<&'a crate::manager::PopOrigin>,
            &'a crate::registry::Registry,
        ),
    >,
) -> &'a str {
    let windows: Vec<_> = windows.into_iter().collect();
    if windows
        .iter()
        .any(|(l, _, reg)| *l == label && reg.holds_tab(tab))
    {
        return label;
    }
    let holds = |(_, _, reg): &&(&str, _, &crate::registry::Registry)| reg.holds_tab(tab);
    windows
        .iter()
        .filter(holds)
        .find(|(_, o, _)| o.is_some_and(|o| o.label == label))
        .or_else(|| windows.iter().find(holds))
        .map_or(label, |(l, _, _)| l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::PopOrigin;
    use crate::registry::Registry;
    use crate::surface::{PixelRect, TabSpec};

    fn spec(id: &str) -> TabSpec {
        TabSpec {
            colour: None,
            id: id.into(),
            title: id.into(),
            dir: std::path::PathBuf::from(id),
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

    fn registry() -> Registry {
        Registry::new(
            std::ptr::null_mut(),
            PixelRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        )
    }

    /// A banner names the window the tab was in when it fired; once the tab is out, the origin
    /// holds only its placeholder and the pop-out holding it is the one raised.
    #[test]
    fn attention_target_for_a_popped_out_tab_is_the_popout() {
        let mut origin = registry();
        let _ = origin.add(&spec("/tmp/a"), false);
        let _ = origin.add(&spec("/tmp/b"), false);
        let moved = origin.take_tab("/tmp/a").expect("the tab moves");
        let mut popout = registry();
        assert!(popout.adopt_tab(moved).is_ok());
        let from = PopOrigin {
            label: "work".into(),
            title: "work".into(),
            tab_id: "/tmp/a".into(),
        };
        let windows = || {
            [
                ("work", None, &origin),
                ("shell-detach:k", Some(&from), &popout),
            ]
        };
        assert_eq!(
            attention_target("work", "/tmp/a", windows()),
            "shell-detach:k"
        );
        assert_eq!(attention_target("work", "/tmp/b", windows()), "work");
        assert_eq!(
            attention_target("shell-detach:k", "/tmp/a", windows()),
            "shell-detach:k"
        );
    }

    /// A banner a pop-out fired, clicked after the pop-out closed and its tab went home, raises the
    /// window holding the tab now — not the pop-out's dead label.
    #[test]
    fn attention_target_for_a_tab_gone_home_is_its_window() {
        let mut origin = registry();
        let _ = origin.add(&spec("/tmp/a"), false);
        let windows = [("work", None, &origin)];
        assert_eq!(
            attention_target("shell-detach:k", "/tmp/a", windows),
            "work"
        );
        let windows = [("work", None, &origin)];
        assert_eq!(
            attention_target("shell-detach:k", "/tmp/gone", windows),
            "shell-detach:k",
            "no window holds it: the label stands"
        );
    }
}
