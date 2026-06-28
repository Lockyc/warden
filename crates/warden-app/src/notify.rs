//! Routes terminal attention signals (bell / desktop notification) from a surface to its tab.
//!
//! The surface seam (`surface::set_surface_event_sink`) hands us a seam-neutral `SurfaceEvent`
//! whenever libghostty reports `RING_BELL` or a `DESKTOP_NOTIFICATION` (OSC 9 / OSC 777). We map
//! the surface back to its (window, tab) via the `WindowManager`, then:
//!   - badge the tab in that window's chrome (`warden:notify`), and
//!   - for a desktop notification, raise a macOS banner —
//! but only when the tab is **not** already visible (focused window + active tab): if you're
//! looking at it, there's nothing to notify. This is what replaces agentmux's direct `osascript`
//! call — the signal now flows through the terminal and lands on the right tab.

use crate::surface::{SurfaceEvent, SurfaceSignal};
use crate::ManagerState;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// Install the surface-signal handler. Called once at setup; captures the `AppHandle` the
/// callback needs to reach the manager + emit events + show banners.
pub fn init(app: AppHandle) {
    crate::surface::set_surface_event_sink(move |event| handle(&app, event));
}

/// Runs on the main thread (the sink is invoked from `action_cb`, itself dispatched on the main
/// runloop), so locking `ManagerState` and touching Tauri/AppKit here is safe.
fn handle(app: &AppHandle, event: SurfaceEvent) {
    let located = app
        .state::<ManagerState>()
        .0
        .lock()
        .unwrap()
        .locate_surface(event.surface_id);
    let Some((label, tab, visible)) = located else {
        return; // surface not found (e.g. just unloaded) — drop the signal
    };
    if visible {
        return; // the user is already looking at this tab
    }

    // Badge the tab in its window's sidebar. Payload is the tab id; the chrome marks it unread
    // until focused. emit_to targets just that window.
    let _ = app.emit_to(label.as_str(), "warden:notify", &tab);

    // A desktop notification (OSC 9/777) additionally raises a macOS banner; a bare bell only
    // badges (no text to show, and bells are frequent enough that banners would be noise).
    if let SurfaceSignal::Notification { title, body } = event.signal {
        let title = if title.trim().is_empty() {
            "warden".to_string()
        } else {
            title
        };
        let _ = app.notification().builder().title(title).body(body).show();
    }
}
