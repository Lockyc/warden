//! The terminal-surface seam (spec §4.1). Callers (`main.rs`) use ONLY the
//! `TerminalSurface` trait + the `GhosttySurface::new` constructor. All
//! libghostty/objc2 calls live behind this boundary in `ghostty.rs` + `ffi`,
//! so a future SwiftTerm / GTK impl can be slotted in without touching callers.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub mod ghostty;

#[derive(Debug, Clone, PartialEq)]
pub struct TabSpec {
    pub id: String,
    pub title: String,
    pub dir: PathBuf,
    pub cmd: String,
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
