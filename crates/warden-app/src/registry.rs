use crate::surface::{ghostty::GhosttySurface, PixelRect, TabSpec, TerminalSurface};
use std::os::raw::c_void;

pub struct Registry {
    surfaces: Vec<(String, GhosttySurface)>, // ordered by tab; key = TabSpec.id
    active: Option<String>,
    last_rect: Option<PixelRect>,
}

impl Registry {
    pub fn new() -> Self {
        Registry { surfaces: Vec::new(), active: None, last_rect: None }
    }

    /// Create a surface for `spec` and insert it (hidden) at the back of the tab list.
    /// `ns_window` is the raw `NSWindow *` from Tauri's `WebviewWindow::ns_window()`;
    /// `GhosttySurface::new` derives the content-view internally (seam constraint).
    pub fn create(&mut self, ns_window: *mut c_void, rect: PixelRect, spec: &TabSpec) {
        let s = GhosttySurface::new(ns_window, rect, spec).expect("surface create");
        s.hide(); // all start hidden; activate() reveals the chosen one
        self.surfaces.push((spec.id.clone(), s));
        self.last_rect = Some(rect);
    }

    /// Hide all surfaces, then show + focus the one matching `id`.
    /// Applies `last_rect` to the target before showing so geometry is correct.
    /// If no surface has `id`, does nothing (does not hide others, does not update `active`).
    pub fn activate(&mut self, id: &str) {
        if !self.surfaces.iter().any(|(sid, _)| sid == id) {
            return;
        }
        for (sid, s) in &self.surfaces {
            if sid == id {
                if let Some(r) = self.last_rect {
                    s.set_frame(r);
                }
                s.show();
                s.focus();
            } else {
                s.hide();
            }
        }
        self.active = Some(id.to_string());
    }

    /// Update the geometry of the active surface; store for hidden surfaces
    /// to receive on their next `activate`.
    pub fn set_active_frame(&mut self, rect: PixelRect) {
        self.last_rect = Some(rect);
        if let Some(ref active) = self.active.clone() {
            if let Some((_, s)) = self.surfaces.iter().find(|(id, _)| id == active) {
                s.set_frame(rect);
            }
        }
    }

    /// Destroy all surfaces (called on window close / app exit).
    pub fn close_all(&mut self) {
        for (_, s) in self.surfaces.drain(..) {
            s.close();
        }
        self.active = None;
    }
}
