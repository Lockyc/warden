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
//! The banner is also **clickable**: each one stamps its originating `(window label, tab id)` into
//! the request's `userInfo`, and the delegate's `did_receive` reads it back on a click to raise that
//! window and select the tab (`focus_window_tab`) — so a notification surfaces the terminal that
//! raised it, even from the background.
//!
//! The banner is posted via the native **UserNotifications** framework
//! (`UNUserNotificationCenter`). The earlier `tauri-plugin-notification` backend went through
//! `notify-rust` → `mac-notification-sys`, which posts via the deprecated `NSUserNotification`
//! API — a silent no-op on macOS 26 (the call succeeds, nothing is delivered, the app never even
//! registers in Notification Center). `UNUserNotificationCenter` is the modern, supported path and
//! posts under warden's own bundle identity. Two requirements it imposes, both handled in
//! `setup_banners`: the app must request authorization once, and — because the system suppresses
//! banners while the posting app is frontmost — a delegate must opt in to presenting them anyway
//! (warden fires banners even when it is the focused app, as long as the *target tab* is hidden).

use crate::surface::{SurfaceEvent, SurfaceSignal};
use crate::ManagerState;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter, Manager};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_foundation::{NSDictionary, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions, UNNotificationRequest,
    UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

/// True once authorization has been requested and a delegate installed — i.e. native banners are
/// usable. Stays false in dev (`cargo run`/`just run`): there is no app bundle, and
/// `UNUserNotificationCenter::currentNotificationCenter` throws on a nil bundle identifier. The
/// badge channel is independent and works regardless.
static BANNER_READY: AtomicBool = AtomicBool::new(false);

/// The app handle, captured at `init`. The notification-click delegate (`did_receive`) runs on the
/// main thread with no context of its own, so it reaches the manager + chrome through this.
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// Monotonic suffix for notification request identifiers, so distinct alerts don't coalesce
/// (a reused identifier *replaces* the pending request rather than stacking a new banner).
static BANNER_SEQ: AtomicU64 = AtomicU64::new(0);

declare_class!(
    /// `UNUserNotificationCenterDelegate` whose sole job is to present banners even while warden
    /// is the frontmost app. Without it, the system silently drops the banner (showing it only in
    /// the Notification Center list) whenever the posting app is foreground — which is exactly the
    /// hidden-tab-in-the-focused-window case warden notifies on.
    struct NotificationDelegate;

    unsafe impl ClassType for NotificationDelegate {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "WardenNotificationDelegate";
    }

    impl DeclaredClass for NotificationDelegate {
        type Ivars = ();
    }

    unsafe impl NotificationDelegate {}

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[method(userNotificationCenter:willPresentNotification:withCompletionHandler:)]
        unsafe fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::Block<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            let opts = UNNotificationPresentationOptions::UNNotificationPresentationOptionBanner
                | UNNotificationPresentationOptions::UNNotificationPresentationOptionSound;
            completion_handler.call((opts,));
        }

        // The user clicked (or dismissed) a banner. On a click — the *default* action — surface the
        // tab that raised it: read the (window label, tab id) we stamped into `userInfo`, raise that
        // window, and tell its chrome to select the tab. Delivered on the main thread, so touching
        // Tauri/AppKit here is safe. We always call the completion handler (the system requires it).
        #[method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:)]
        unsafe fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::Block<dyn Fn()>,
        ) {
            // Ignore dismiss / custom actions — only a tap on the banner body surfaces the tab.
            if &*response.actionIdentifier() == UNNotificationDefaultActionIdentifier {
                let content = response.notification().request().content();
                let user_info = content.userInfo();
                if let Some(label) = dict_str(&user_info, "label") {
                    focus_window_tab(label, dict_str(&user_info, "id"));
                }
            }
            completion_handler.call(());
        }
    }
);

/// Read a string value out of a notification's `userInfo`. The dictionary is untyped
/// (`NSDictionary<AnyObject, AnyObject>`); we stamped only `NSString → NSString` pairs into it
/// (`show_banner`), so casting to that concrete type and looking the key up is sound.
unsafe fn dict_str(user_info: &NSDictionary, key: &str) -> Option<String> {
    let typed: &NSDictionary<NSString, NSString> =
        &*(user_info as *const NSDictionary as *const NSDictionary<NSString, NSString>);
    typed
        .get_retained(&NSString::from_str(key))
        .map(|s| s.to_string())
}

/// Raise the window owning a notification and ask its chrome to select the tab. Best-effort: an
/// unknown label (window since closed) raises nothing; a stale/missing tab id leaves the active tab
/// alone, so the click still lands you in the right window. `set_focus` brings the window forward
/// and activates warden, so this surfaces the terminal even from the background. `emit_to` leaks to
/// sibling webviews (see CLAUDE.md), so the payload carries the target `label` and the chrome drops
/// events meant for another window — same guard as every other per-window event.
fn focus_window_tab(label: String, id: Option<String>) {
    let Some(app) = APP_HANDLE.get() else {
        return;
    };
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    let _ = app.emit_to(
        label.as_str(),
        "warden:focus-tab",
        serde_json::json!({ "label": label, "id": id }),
    );
}

/// Install the surface-signal handler and prime native banners. Called once at setup (on the main
/// thread); captures the `AppHandle` the callback needs to reach the manager + emit chrome events.
pub fn init(app: AppHandle) {
    let _ = APP_HANDLE.set(app.clone());
    crate::surface::set_surface_event_sink(move |event| handle(&app, event));
    setup_banners();
}

/// Request notification authorization once and install the presentation delegate. No-op in dev
/// (no bundle → `currentNotificationCenter` would throw); native banners are only expected from the
/// packaged `warden.app`. Runs on the main thread (Tauri's setup hook).
fn setup_banners() {
    if tauri::is_dev() {
        return;
    }
    let center = unsafe { UNUserNotificationCenter::currentNotificationCenter() };

    // The center holds its delegate weakly, so the instance must outlive setup — leak it as an
    // app-lifetime singleton (it carries no state and is never torn down before exit).
    let delegate: Retained<NotificationDelegate> =
        unsafe { msg_send_id![NotificationDelegate::alloc(), init] };
    unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    std::mem::forget(delegate);

    let opts = UNAuthorizationOptions::UNAuthorizationOptionAlert
        | UNAuthorizationOptions::UNAuthorizationOptionSound;
    // Async; the system shows its one-time prompt on first launch. Heap block (RcBlock) because it
    // escapes the call.
    let handler = RcBlock::new(|granted: Bool, _err: *mut NSError| {
        if !granted.as_bool() {
            eprintln!(
                "warden: notification authorization not granted — banners will be suppressed"
            );
        }
    });
    unsafe { center.requestAuthorizationWithOptions_completionHandler(opts, &handler) };

    BANNER_READY.store(true, Ordering::Release);
}

/// Runs on the main thread (the sink is invoked from `action_cb`, which only ever fires from a
/// `ghostty_app_tick` that is *async-dispatched* onto the main queue — never synchronously from a
/// surface method). So this never nests inside a command that already holds `ManagerState`: the
/// command's main-queue task runs to completion (and drops its guard) before this tick task runs.
/// Locking `ManagerState` and touching Tauri/AppKit here is therefore safe and deadlock-free.
/// Uses `ManagerState::lock` (not a bare `unwrap`) so a poisoned mutex recovers instead of
/// crashing the notification path — matching every command handler.
fn handle(app: &AppHandle, event: SurfaceEvent) {
    let located = app
        .state::<ManagerState>()
        .lock()
        .locate_surface(event.surface_id);
    let Some((label, tab, visible)) = located else {
        return; // surface not found (e.g. just unloaded) — drop the signal
    };
    if visible {
        return; // the user is already looking at this tab
    }

    // Badge the tab in its window's sidebar; the chrome marks it unread until focused. emit_to
    // leaks to sibling windows here (see CLAUDE.md), so stamp the target window label into the
    // payload and let the chrome drop events meant for a sibling — same guard as every other
    // per-window event. Without it, a hidden tab's bell badges a same-titled tab in another window.
    let _ = app.emit_to(
        label.as_str(),
        "warden:notify",
        serde_json::json!({ "label": label, "id": tab }),
    );

    // A desktop notification (OSC 9/777) additionally raises a macOS banner; a bare bell only
    // badges (no text to show, and bells are frequent enough that banners would be noise).
    if let SurfaceSignal::Notification { title, body } = event.signal {
        let title = if title.trim().is_empty() {
            "warden".to_string()
        } else {
            title
        };
        show_banner(&title, &body, &label, &tab);
    }
}

/// Post a native banner via `UNUserNotificationCenter`. No-op until `setup_banners` has run
/// (dev, or before setup). Main thread only. `label`/`tab` are stamped into the request's
/// `userInfo` so a click can route back to the originating window + tab (`did_receive`).
fn show_banner(title: &str, body: &str, label: &str, tab: &str) {
    if !BANNER_READY.load(Ordering::Acquire) {
        return;
    }
    let center = unsafe { UNUserNotificationCenter::currentNotificationCenter() };
    let content: Retained<UNMutableNotificationContent> =
        unsafe { msg_send_id![UNMutableNotificationContent::alloc(), init] };
    unsafe {
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        let keys = [NSString::from_str("label"), NSString::from_str("id")];
        let vals = vec![NSString::from_str(label), NSString::from_str(tab)];
        let user_info: Retained<NSDictionary<NSString, NSString>> =
            NSDictionary::from_vec(&[&*keys[0], &*keys[1]], vals);
        // setUserInfo wants the untyped NSDictionary<AnyObject, AnyObject>; our typed dict has the
        // same layout (phantom key/value types), so the cast is sound.
        let user_info: Retained<NSDictionary> = Retained::cast(user_info);
        content.setUserInfo(&user_info);
    }
    let id = BANNER_SEQ.fetch_add(1, Ordering::Relaxed);
    let ident = NSString::from_str(&format!("warden-{id}"));
    // nil trigger → deliver immediately; nil completion handler → fire-and-forget.
    let request = unsafe {
        UNNotificationRequest::requestWithIdentifier_content_trigger(&ident, &content, None)
    };
    unsafe { center.addNotificationRequest_withCompletionHandler(&request, None) };
}
