//! Opening a clicked link (`GHOSTTY_ACTION_OPEN_URL`) — a port of Ghostty.app's policy
//! (`macos/Sources/Helpers/UntrustedURL.swift`, `UntrustedURLAlert.swift`, and
//! `Ghostty.App.swift::openURL`), because the policy lives in Ghostty's Swift app, not in
//! libghostty: an embedder gets the raw target and must screen it itself.
//!
//! The `kind` says where the target came from. `text`/`html` are libghostty's own (e.g. opening the
//! config) and open directly. `osc8` (a hyperlink's target, exactly as the program sent it — the
//! visible label may not match) and `unknown` (link-regex text matched in terminal output) are
//! producer-controlled, so they go through [`decide`]: http/https/mailto open, a custom scheme asks
//! first, and malformed targets, invisible/bidi characters and anything that could execute code
//! (an `.app`, a script, an executable file) are refused with an alert. A future kind value is
//! treated as untrusted.
//!
//! Every Foundation call here is main-thread-agnostic except the alerts, which [`open`] defers to
//! the next main-queue turn: `action_cb` runs inside `ghostty_app_tick` with libghostty's renderer
//! mutex held, and a modal run loop must not re-enter a render callback before it is released.

use std::os::raw::c_void;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, msg_send_id, ClassType};
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSFont, NSModalResponse, NSPasteboard,
    NSPasteboardTypeString, NSTextField, NSView, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSCopying, NSSize, NSString, NSURLComponents, NSURLContentTypeKey,
    NSURLIsDirectoryKey, NSURLIsExecutableKey, NSURLIsRegularFileKey, NSURLResourceKey, NSURL,
};

use crate::ffi;

// The classes `decide` asks about by UTI live in this framework; AppKit doesn't guarantee to link it.
#[link(name = "UniformTypeIdentifiers", kind = "framework")]
extern "C" {}

/// Why an untrusted target was refused — shown as the block alert's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    MalformedUrl,
    UnsafeCharacters,
    InvalidWebUrl,
    InaccessibleFile,
    UnsafeFile,
}

impl DenialReason {
    fn message(self) -> &'static str {
        match self {
            Self::MalformedUrl => "The target is not an absolute URL with a scheme.",
            Self::UnsafeCharacters => "The target contains invisible or line-breaking characters.",
            Self::InvalidWebUrl => "The web target does not contain a valid host.",
            Self::InaccessibleFile => {
                "The local target does not exist or is not a regular file or directory."
            }
            Self::UnsafeFile => "Opening this local target could execute code.",
        }
    }
}

/// What to do with an untrusted target.
pub enum Decision {
    /// A scheme with non-executing, well-understood behaviour — open it.
    Allow(Retained<NSURL>),
    /// A custom scheme can launch any app registered for it — show the target and handler first.
    Confirm(Retained<NSURL>),
    /// Never dispatched.
    Deny(DenialReason),
}

/// Open the link libghostty raised for a click. Main thread (called from `action_cb`).
pub fn open(kind: u32, raw: &str) {
    match kind {
        ffi::GHOSTTY_ACTION_OPEN_URL_KIND_TEXT | ffi::GHOSTTY_ACTION_OPEN_URL_KIND_HTML => {
            open_trusted(raw)
        }
        ffi::GHOSTTY_ACTION_OPEN_URL_KIND_UNKNOWN => open_untrusted(&unknown_target(raw)),
        _ => open_untrusted(raw),
    }
}

/// A target libghostty produced itself. A scheme-less value is a file path (`~` expanded) —
/// `NSURL` would otherwise accept it as a scheme-less URL that opens nowhere.
fn open_trusted(raw: &str) {
    unsafe {
        let s = NSString::from_str(raw);
        let url = match NSURL::URLWithString(&s) {
            Some(u) if u.scheme().is_some() => u,
            _ => NSURL::fileURLWithPath(&s.stringByStandardizingPath()),
        };
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

/// An `unknown`-kind target may be a plain path (`~/notes.txt`, `src/main.rs`), which [`decide`]
/// rejects as scheme-less, so a path becomes the file URL it names first. A value with unsafe
/// characters passes through unchanged so `decide` rejects it before percent-encoding hides them.
fn unknown_target(raw: &str) -> String {
    let has_scheme = unsafe {
        NSURL::URLWithString(&NSString::from_str(raw)).is_some_and(|u| u.scheme().is_some())
    };
    if has_scheme || contains_unsafe_characters(raw) {
        return raw.to_owned();
    }
    unsafe {
        let path = NSString::from_str(raw).stringByStandardizingPath();
        NSURL::fileURLWithPath(&path)
            .absoluteString()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

fn open_untrusted(raw: &str) {
    match decide(raw) {
        Decision::Allow(url) => unsafe {
            NSWorkspace::sharedWorkspace().openURL(&url);
        },
        Decision::Confirm(url) => defer(Pending::Confirm {
            url,
            display: display_string(raw),
        }),
        Decision::Deny(reason) => defer(Pending::Block {
            reason,
            display: display_string(raw),
        }),
    }
}

/// Classify an untrusted target. Pure apart from the filesystem lookups a `file:` target needs.
pub fn decide(s: &str) -> Decision {
    if s.is_empty() {
        return Decision::Deny(DenialReason::MalformedUrl);
    }
    // NSURL accepts many control and formatting characters that UI text renders as line breaks,
    // zero-width text or bidi overrides — reject them before parsing changes their representation.
    if contains_unsafe_characters(s) {
        return Decision::Deny(DenialReason::UnsafeCharacters);
    }
    // NSURL also accepts relative references; an untrusted target must carry an explicit scheme so
    // no later layer can reinterpret it as a local path.
    let Some(url) = (unsafe { NSURL::URLWithString(&NSString::from_str(s)) }) else {
        return Decision::Deny(DenialReason::MalformedUrl);
    };
    let scheme = unsafe { url.scheme() }
        .map(|s| s.to_string().to_ascii_lowercase())
        .unwrap_or_default();
    match scheme.as_str() {
        "" => Decision::Deny(DenialReason::MalformedUrl),
        // `https:relative` has a scheme but no authority, and consumers resolve it differently.
        "http" | "https" => match unsafe { url.host() } {
            Some(h) if !h.to_string().is_empty() => Decision::Allow(url),
            _ => Decision::Deny(DenialReason::InvalidWebUrl),
        },
        // The address is the components' path; a bare `mailto:` would open an empty draft.
        "mailto" => {
            let path = unsafe {
                NSURLComponents::componentsWithURL_resolvingAgainstBaseURL(&url, false)
                    .and_then(|c| c.path())
            };
            match path {
                Some(p) if !p.to_string().is_empty() => Decision::Allow(url),
                _ => Decision::Deny(DenialReason::MalformedUrl),
            }
        }
        "file" => file_decision(&url),
        _ => Decision::Confirm(url),
    }
}

fn file_decision(url: &NSURL) -> Decision {
    unsafe {
        // A query or fragment names no part of a filesystem object, and Launch Services handlers
        // read them inconsistently.
        if !url.isFileURL() || url.query().is_some() || url.fragment().is_some() {
            return Decision::Deny(DenialReason::MalformedUrl);
        }
        // An empty host and `localhost` are this machine; any other host could reach the network.
        if let Some(host) = url.host() {
            let host = host.to_string();
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return Decision::Deny(DenialReason::MalformedUrl);
            }
        }
        // Classify the effective object, not the spelling terminal output supplied: this collapses
        // dot traversal and stops a harmless-looking symlink from hiding an executable target.
        let Some(canonical) = url
            .URLByStandardizingPath()
            .and_then(|u| u.URLByResolvingSymlinksInPath())
        else {
            return Decision::Deny(DenialReason::InaccessibleFile);
        };
        let keys: Retained<NSArray<NSURLResourceKey>> = NSArray::from_vec(
            [
                NSURLContentTypeKey,
                NSURLIsDirectoryKey,
                NSURLIsExecutableKey,
                NSURLIsRegularFileKey,
            ]
            .map(|k| k.copy())
            .into(),
        );
        // Reading the keys also proves the canonical target exists and is accessible.
        let Ok(values) = canonical.resourceValuesForKeys_error(&keys) else {
            return Decision::Deny(DenialReason::InaccessibleFile);
        };
        let flag = |key: &NSURLResourceKey| -> bool {
            values
                .get(key)
                .is_some_and(|v| msg_send![v, boolValue])
        };
        let is_dir = flag(NSURLIsDirectoryKey);
        // Devices, sockets and other special objects are refused outright.
        if !is_dir && !flag(NSURLIsRegularFileKey) {
            return Decision::Deny(DenialReason::InaccessibleFile);
        }
        let content_type = values.get(NSURLContentTypeKey);
        if is_unsafe_file(&canonical, content_type, is_dir, flag(NSURLIsExecutableKey)) {
            return Decision::Deny(DenialReason::UnsafeFile);
        }
        Decision::Allow(canonical)
    }
}

/// Launch Services picks a handler by extension, so known executable containers are refused even
/// with the execute bit clear; the UTI catches a missing or misleading extension; and any regular
/// file the filesystem marks executable is refused whatever its name. A directory is fine to reveal
/// in Finder unless its extension or UTI makes it an application bundle.
fn is_unsafe_file(
    url: &NSURL,
    content_type: Option<&AnyObject>,
    is_dir: bool,
    is_executable: bool,
) -> bool {
    let ext = unsafe { url.pathExtension() }
        .map(|e| e.to_string().to_ascii_lowercase())
        .unwrap_or_default();
    if UNSAFE_PATH_EXTENSIONS.contains(&ext.as_str()) {
        return true;
    }
    if let (Some(ct), Some(cls)) = (content_type, AnyClass::get("UTType")) {
        for id in UNSAFE_CONTENT_TYPES {
            let ty: Option<Retained<AnyObject>> =
                unsafe { msg_send_id![cls, typeWithIdentifier: &*NSString::from_str(id)] };
            if let Some(ty) = ty {
                let conforms: bool = unsafe { msg_send![ct, conformsToType: &*ty] };
                if conforms {
                    return true;
                }
            }
        }
    }
    !is_dir && is_executable
}

/// C0/C1 controls (CR, LF, NEL, …), directional marks and zero-width characters (which reorder or
/// hide part of a target without changing what the handler receives), Unicode line/paragraph
/// separators (an extra visual line in AppKit text), and the invisible word joiner / BOM.
fn is_unsafe_character(c: char) -> bool {
    matches!(
        c as u32,
        0x00..=0x1F
            | 0x7F..=0x9F
            | 0x061C
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2066..=0x2069
            | 0x2028..=0x2029
            | 0x2060
            | 0xFEFF
    )
}

fn contains_unsafe_characters(s: &str) -> bool {
    s.chars().any(is_unsafe_character)
}

/// The effective target on one line, for the alerts. A file URL is standardized and
/// symlink-resolved exactly as it is before opening, so traversal and repeated separators can't make
/// the shown and opened targets differ; other URLs stay byte-for-byte (separators can matter to a
/// handler). Unsafe characters left after normalizing are escaped as `\u{…}` text.
fn display_string(s: &str) -> String {
    let normalized = unsafe {
        let ns = NSString::from_str(s);
        match NSURL::URLWithString(&ns) {
            Some(u) if u.scheme().is_some() => {
                if u.isFileURL() {
                    u.URLByStandardizingPath()
                        .and_then(|u| u.URLByResolvingSymlinksInPath())
                        .and_then(|u| u.path())
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| s.to_owned())
                } else {
                    s.to_owned()
                }
            }
            // Scheme-less values never open, but still appear in the block alert.
            _ => NSURL::fileURLWithPath(&ns)
                .URLByStandardizingPath()
                .and_then(|u| u.path())
                .map(|p| p.to_string())
                .unwrap_or_else(|| s.to_owned()),
        }
    };
    let mut out = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        if is_unsafe_character(c) {
            out.push_str(&format!("\\u{{{:X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

const UNSAFE_PATH_EXTENSIONS: &[&str] = &[
    "action",
    "app",
    "applescript",
    "class",
    "command",
    "desktop",
    "inetloc",
    "jar",
    "mobileconfig",
    "mpkg",
    "pkg",
    "scpt",
    "terminal",
    "tool",
    "url",
    "webloc",
    "workflow",
];

/// Broad system types, so subclasses (shell scripts, app bundles) conform automatically.
const UNSAFE_CONTENT_TYPES: &[&str] = &["com.apple.application", "public.executable", "public.script"];

// --- Alerts ----------------------------------------------------------------------------------------

enum Pending {
    Confirm {
        url: Retained<NSURL>,
        display: String,
    },
    Block {
        reason: DenialReason,
        display: String,
    },
}

fn defer(p: Pending) {
    unsafe {
        super::ghostty::dispatch_async_f(
            super::ghostty::main_queue(),
            Box::into_raw(Box::new(p)) as *mut c_void,
            present_trampoline,
        );
    }
}

unsafe extern "C" fn present_trampoline(context: *mut c_void) {
    let pending = *Box::from_raw(context as *mut Pending);
    // Main-queue work items run on the main thread.
    let mtm = MainThreadMarker::new_unchecked();
    match pending {
        Pending::Confirm { url, display } => present_confirmation(mtm, url, &display),
        Pending::Block { reason, display } => present_block(mtm, reason, &display),
    }
}

// NSAlert's first and second button responses (NSAlertFirstButtonReturn = 1000).
const SECOND_BUTTON: NSModalResponse = 1001;

unsafe fn present_confirmation(mtm: MainThreadMarker, url: Retained<NSURL>, display: &str) {
    let workspace = NSWorkspace::sharedWorkspace();
    let handler = workspace
        .URLForApplicationToOpenURL(&url)
        .and_then(|app| app.URLByDeletingPathExtension())
        .and_then(|app| app.lastPathComponent())
        .map(|name| format!("\u{201c}{name}\u{201d}"))
        .unwrap_or_else(|| "the default application".to_owned());
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str("Open Link from Terminal Output?"));
    alert.setInformativeText(&NSString::from_str(&format!(
        "This link will open in {handler}. Only continue if you recognize and trust the destination."
    )));
    alert.setAccessoryView(Some(&target_view(mtm, display)));
    // Cancel first, so it is the default (Return) action.
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.addButtonWithTitle(&NSString::from_str("Open Link"));
    present(mtm, &alert, move |response| {
        if response == SECOND_BUTTON {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    });
}

unsafe fn present_block(mtm: MainThreadMarker, reason: DenialReason, display: &str) {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str("warden Blocked This Link"));
    alert.setInformativeText(&NSString::from_str(reason.message()));
    alert.setAccessoryView(Some(&target_view(mtm, display)));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.addButtonWithTitle(&NSString::from_str("Copy Link"));
    // Copying the shown, sanitized value is the way forward — never a one-click policy bypass.
    let display = display.to_owned();
    present(mtm, &alert, move |response| {
        if response == SECOND_BUTTON {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            pb.setString_forType(&NSString::from_str(&display), NSPasteboardTypeString);
        }
    });
}

/// A sheet on the key window when there is one, else app-modal.
unsafe fn present(
    mtm: MainThreadMarker,
    alert: &NSAlert,
    done: impl Fn(NSModalResponse) + 'static,
) {
    match NSApplication::sharedApplication(mtm).keyWindow() {
        Some(window) => {
            let block = RcBlock::new(done);
            alert.beginSheetModalForWindow_completionHandler(&window, Some(&block));
        }
        None => done(alert.runModal()),
    }
}

/// The target, selectable and monospaced, set apart from the alert's prose.
unsafe fn target_view(mtm: MainThreadMarker, display: &str) -> Retained<NSView> {
    const WIDTH: f64 = 480.0;
    let field = NSTextField::wrappingLabelWithString(&NSString::from_str(display), mtm);
    field.setSelectable(true);
    let size = NSFont::systemFontSize();
    let font: Retained<NSFont> =
        msg_send_id![NSFont::class(), monospacedSystemFontOfSize: size, weight: 0.0f64];
    field.setFont(Some(&font));
    field.setPreferredMaxLayoutWidth(WIDTH);
    let fit: NSSize = msg_send![&field, fittingSize];
    field.setFrameSize(NSSize::new(WIDTH, fit.height));
    Retained::into_super(Retained::into_super(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(d: &Decision) -> Option<String> {
        match d {
            Decision::Allow(u) => Some(unsafe { u.absoluteString() }.unwrap().to_string()),
            _ => None,
        }
    }
    fn denied(d: &Decision) -> Option<DenialReason> {
        match d {
            Decision::Deny(r) => Some(*r),
            _ => None,
        }
    }

    #[test]
    fn web_and_mail_targets_open() {
        assert_eq!(
            allowed(&decide("https://example.com/x?a=1")).as_deref(),
            Some("https://example.com/x?a=1")
        );
        assert!(allowed(&decide("HTTP://example.com")).is_some());
        assert!(allowed(&decide("mailto:someone@example.com")).is_some());
    }

    #[test]
    fn malformed_targets_are_refused() {
        assert_eq!(denied(&decide("")), Some(DenialReason::MalformedUrl));
        assert_eq!(denied(&decide("src/main.rs")), Some(DenialReason::MalformedUrl));
        assert_eq!(denied(&decide("https:relative")), Some(DenialReason::InvalidWebUrl));
        assert_eq!(denied(&decide("mailto:")), Some(DenialReason::MalformedUrl));
        assert_eq!(
            denied(&decide("file://evil.example/etc/hosts")),
            Some(DenialReason::MalformedUrl)
        );
        assert_eq!(denied(&decide("file:///etc/hosts#x")), Some(DenialReason::MalformedUrl));
    }

    #[test]
    fn invisible_and_bidi_characters_are_refused() {
        for s in [
            "https://example.com/\u{202E}gpj.exe",
            "https://exa\u{200B}mple.com",
            "https://example.com/\nsecond-line",
            "https://example.com/\u{FEFF}",
        ] {
            assert_eq!(denied(&decide(s)), Some(DenialReason::UnsafeCharacters), "{s:?}");
        }
    }

    #[test]
    fn custom_schemes_ask_first() {
        assert!(matches!(decide("x-warden-test://do?thing=1"), Decision::Confirm(_)));
        assert!(matches!(decide("vscode://file/tmp/a"), Decision::Confirm(_)));
    }

    #[test]
    fn executable_local_targets_are_refused() {
        // An app bundle by extension, even though it is "just" a directory.
        assert_eq!(
            denied(&decide("file:///System/Applications/Calculator.app")),
            Some(DenialReason::UnsafeFile)
        );
        let dir = tempfile::tempdir().unwrap();
        // A script by extension with no execute bit.
        let cmd = dir.path().join("run.command");
        std::fs::write(&cmd, "echo hi\n").unwrap();
        // An executable regular file with a harmless-looking name.
        let exe = dir.path().join("notes.txt");
        std::fs::write(&exe, "plain\n").unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        // A harmless name symlinked at an executable target.
        let link = dir.path().join("readme");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        for p in [&cmd, &exe, &link] {
            let url = format!("file://{}", p.display());
            assert_eq!(denied(&decide(&url)), Some(DenialReason::UnsafeFile), "{url}");
        }
    }

    #[test]
    fn plain_local_files_and_directories_open() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "plain\n").unwrap();
        assert!(allowed(&decide(&format!("file://{}", txt.display()))).is_some());
        assert!(allowed(&decide(&format!("file://{}/", dir.path().display()))).is_some());
        assert!(allowed(&decide(&format!("file://localhost{}", txt.display()))).is_some());
        let missing = format!("file://{}/nope", dir.path().display());
        assert_eq!(denied(&decide(&missing)), Some(DenialReason::InaccessibleFile));
    }

    #[test]
    fn unknown_kind_paths_become_file_urls() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(unknown_target("~/notes.txt"), format!("file://{home}/notes.txt"));
        assert_eq!(unknown_target("https://example.com"), "https://example.com");
        // Left raw so `decide` still sees the character.
        assert_eq!(unknown_target("a\u{202E}b"), "a\u{202E}b");
    }

    #[test]
    fn display_string_shows_the_effective_target_on_one_line() {
        // Resolving symlinks also strips a leading `/private`, exactly as before opening.
        assert_eq!(display_string("file:///tmp/../etc//hosts"), "/etc/hosts");
        assert_eq!(display_string("https://example.com//a"), "https://example.com//a");
        assert_eq!(display_string("a\u{202E}b"), {
            let cwd = std::env::current_dir().unwrap();
            format!("{}/a\\u{{202E}}b", cwd.display())
        });
    }
}
