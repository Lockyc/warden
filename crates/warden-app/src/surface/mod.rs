//! The terminal-surface seam (spec §4.1). Callers (`main.rs`) use ONLY the
//! `TerminalSurface` trait + the `GhosttySurface::new` constructor. All
//! libghostty/objc2 calls live behind this boundary in `ghostty.rs` + `ffi`,
//! so a future SwiftTerm / GTK impl can be slotted in without touching callers.

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
pub mod ghostty;

/// An attention signal a surface raised, in seam-neutral terms (no libghostty/Tauri types).
/// The surface layer decodes the platform action into this; the app layer routes it to a tab.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSignal {
    /// The terminal rang its bell (`\a`). Carries no text.
    Bell,
    /// A desktop-notification escape (OSC 9 / OSC 777). `title`/`body` are whatever the program
    /// emitted (either may be empty).
    Notification { title: String, body: String },
}

/// A signal from a specific surface. `surface_id` is the opaque surface handle as a `usize`
/// (`GhosttySurface::id`), which the app layer maps back to a (window, tab).
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceEvent {
    pub surface_id: usize,
    pub signal: SurfaceSignal,
}

type SurfaceEventSink = Box<dyn Fn(SurfaceEvent) + Send + Sync>;
static SURFACE_EVENT_SINK: OnceLock<SurfaceEventSink> = OnceLock::new();

/// Install the app-level handler for surface signals (bell / desktop notification). Called once
/// at setup; the surface layer stays ignorant of what happens with the events (seam boundary).
pub(crate) fn set_surface_event_sink(sink: impl Fn(SurfaceEvent) + Send + Sync + 'static) {
    let _ = SURFACE_EVENT_SINK.set(Box::new(sink));
}

/// Forward a decoded signal to the installed sink (no-op if none is set, e.g. in tests).
pub(crate) fn emit_surface_event(event: SurfaceEvent) {
    if let Some(sink) = SURFACE_EVENT_SINK.get() {
        sink(event);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabSpec {
    pub id: String,
    pub title: String,
    pub dir: PathBuf,
    /// The shell to exec (an interactive shell under the PTY, e.g. `"fish -l"`).
    pub shell: String,
    /// Optional command auto-run inside the shell on spawn (delivered as libghostty
    /// `initial_input` — typed into the shell, not exec'd). `None` = bare shell.
    pub startup: Option<String>,
    /// The `[[window.group]]` this tab belongs to, or `None` for a loose tab. Carried
    /// only so the registry can pass it to the chrome DTO for sidebar sectioning; the
    /// surface layer itself never reads it.
    pub group: Option<String>,
    /// Optional session-presence probe command (cascaded in resolve). The probe
    /// runner (`probe.rs`) runs it per tab; `None` = no session dot. Opaque here.
    pub probe: Option<String>,
    /// Optional session-kill command (cascaded in resolve). The app runs it via
    /// `sh -c` when the user confirms killing this tab's session. `None` = no kill
    /// affordance. Opaque here — same shape as `probe`.
    pub kill: Option<String>,
}

/// Rect in AppKit view coordinates (points, origin bottom-left).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The swappable terminal-surface abstraction (spec §4.1). libghostty is one impl;
/// SwiftTerm / a Linux GTK widget are future impls. Callers use ONLY these methods.
pub trait TerminalSurface {
    fn set_frame(&self, rect: PixelRect);
    fn show(&self);
    fn hide(&self);
    fn focus(&self);
    /// Type `cmd` into the live shell and submit it — inject the command text, then a real Enter
    /// keypress. Used to re-run a tab's command in its existing shell (e.g. restart a session whose
    /// probe reports it gone) without respawning. The Enter must be a synthesized *key event*, not a
    /// trailing `"\n"` in the text: text injection lands as a paste, and a shell in bracketed-paste
    /// mode inserts the newline literally instead of running the line.
    fn run_command(&self, cmd: &str);
    fn close(self);
}

#[derive(Debug)]
pub enum SurfaceError {
    /// `ghostty_app_new` returned null — the shared app could not be created.
    AppCreateFailed,
    /// `ghostty_surface_new` returned null.
    SurfaceCreateFailed,
}
impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SurfaceError::AppCreateFailed => write!(f, "libghostty app creation failed"),
            SurfaceError::SurfaceCreateFailed => write!(f, "libghostty surface creation failed"),
        }
    }
}
impl std::error::Error for SurfaceError {}
