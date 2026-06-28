use crate::surface::{ghostty::GhosttySurface, PixelRect, TabSpec, TerminalSurface};
use std::os::raw::c_void;

/// Display descriptor sent to the web chrome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TabDto {
    pub id: String,
    pub title: String,
    pub warn: bool,    // dir missing at materialize time
    pub spawned: bool, // surface is live (keep_alive or already focused) vs cold/declared
}

/// A tab's surface is either live or cold (cold = not yet spawned, or unloaded).
/// The `TabSpec` lives on `TabEntry`, not in the slot, so a cold tab always retains
/// what it needs to (re)spawn — `unload` returns a live tab to `Cold` without
/// losing its spec.
enum TabSlot {
    Spawned(GhosttySurface),
    Cold,
}

struct TabEntry {
    id: String,
    title: String,
    warn: bool,
    spec: TabSpec,
    slot: TabSlot,
}

pub struct Registry {
    ns_window: *mut c_void,
    tabs: Vec<TabEntry>,
    active: Option<String>,
    last_rect: PixelRect,
}

// SAFETY: `ns_window` is a raw `NSWindow *` that is only ever read on the main
// thread (Tauri commands + setup all run there). The Mutex in ManagerState enforces
// exclusive access; nothing in Registry sends the pointer across threads.
unsafe impl Send for Registry {}

impl Registry {
    pub fn new(ns_window: *mut c_void, initial_rect: PixelRect) -> Self {
        Registry {
            ns_window,
            tabs: Vec::new(),
            active: None,
            last_rect: initial_rect,
        }
    }

    /// Add a tab. `keep_alive=true` spawns now (eager); `false` declares it
    /// (lazy — spawns on first `activate`). [spec §3]
    pub fn add(&mut self, spec: &TabSpec, keep_alive: bool) {
        let warn = !spec.dir.exists();
        let slot = if keep_alive {
            let s =
                GhosttySurface::new(self.ns_window, self.last_rect, spec).expect("surface create");
            s.hide();
            TabSlot::Spawned(s)
        } else {
            TabSlot::Cold
        };
        self.tabs.push(TabEntry {
            id: spec.id.clone(),
            title: spec.title.clone(),
            warn,
            spec: spec.clone(),
            slot,
        });
    }

    #[cfg(test)]
    pub fn is_spawned(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && matches!(t.slot, TabSlot::Spawned(_)))
    }

    pub fn tab_dtos(&self) -> Vec<TabDto> {
        self.tabs
            .iter()
            .map(|t| TabDto {
                id: t.id.clone(),
                title: t.title.clone(),
                warn: t.warn,
                spawned: matches!(t.slot, TabSlot::Spawned(_)),
            })
            .collect()
    }

    /// Ensure the entry at `idx` is spawned (lazy materialization). A cold tab —
    /// never-opened or previously unloaded — spawns a fresh surface from its spec.
    fn ensure_spawned(&mut self, idx: usize) {
        if let TabSlot::Cold = self.tabs[idx].slot {
            let s = GhosttySurface::new(self.ns_window, self.last_rect, &self.tabs[idx].spec)
                .expect("surface create");
            self.tabs[idx].slot = TabSlot::Spawned(s);
        }
    }

    /// The id of the tab to make visible when the one at `idx` is killed: prefer the
    /// next tab, else the previous, else `None` (it was the only tab). Index-based,
    /// since `unload` leaves the killed entry in place (now cold).
    fn neighbor_id(&self, idx: usize) -> Option<String> {
        self.tabs
            .get(idx + 1)
            .or_else(|| idx.checked_sub(1).and_then(|p| self.tabs.get(p)))
            .map(|t| t.id.clone())
    }

    /// Kill tab `id`'s surface + PTY, returning it to cold (it respawns a fresh
    /// shell on next focus, exactly like a never-opened tab). No-op if the tab is
    /// unknown or already cold. If the killed tab was active, activate a neighbor so
    /// the hole never goes blank, and return that neighbor's id for the chrome to
    /// move its highlight to; otherwise return `None`.
    pub fn unload(&mut self, id: &str) -> Option<String> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        match std::mem::replace(&mut self.tabs[idx].slot, TabSlot::Cold) {
            TabSlot::Spawned(s) => s.close(),
            TabSlot::Cold => return None, // nothing live to kill
        }
        if self.active.as_deref() == Some(id) {
            self.active = None;
            if let Some(next) = self.neighbor_id(idx) {
                self.activate(&next);
                return Some(next);
            }
        }
        None
    }

    /// Show + focus the tab `id` (spawning it first if declared); hide all others.
    pub fn activate(&mut self, id: &str) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.ensure_spawned(idx);
        let rect = self.last_rect;
        for (i, t) in self.tabs.iter().enumerate() {
            if let TabSlot::Spawned(s) = &t.slot {
                if i == idx {
                    s.set_frame(rect);
                    s.show();
                    s.focus();
                } else {
                    s.hide();
                }
            }
        }
        self.active = Some(id.to_string());
    }

    /// Update the geometry of the active surface; store for hidden surfaces
    /// to receive on their next `activate`.
    pub fn set_active_frame(&mut self, rect: PixelRect) {
        self.last_rect = rect;
        if let Some(active) = self.active.clone() {
            if let Some(t) = self.tabs.iter().find(|t| t.id == active) {
                if let TabSlot::Spawned(s) = &t.slot {
                    s.set_frame(rect);
                }
            }
        }
    }

    /// Remove a tab; close its surface if spawned.
    pub fn remove(&mut self, id: &str) {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            let entry = self.tabs.remove(pos);
            if let TabSlot::Spawned(s) = entry.slot {
                s.close();
            }
            if self.active.as_deref() == Some(id) {
                self.active = None;
            }
        }
    }

    /// Reorder entries to match `order` (by id). Ids not in `order` keep their
    /// relative order, appended after the ordered ones.
    pub fn reorder(&mut self, order: &[String]) {
        self.tabs
            .sort_by_key(|t| order.iter().position(|o| o == &t.id).unwrap_or(usize::MAX));
    }

    /// Destroy all surfaces (called on window close / app exit).
    pub fn close_all(&mut self) {
        for entry in self.tabs.drain(..) {
            if let TabSlot::Spawned(s) = entry.slot {
                s.close();
            }
        }
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::{PixelRect, TabSpec};
    use std::path::PathBuf;

    fn rect() -> PixelRect {
        PixelRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        }
    }
    fn spec(id: &str, dir: &str) -> TabSpec {
        TabSpec {
            id: id.into(),
            title: id.into(),
            dir: PathBuf::from(dir),
            shell: "fish".into(),
            startup: None,
        }
    }

    #[test]
    fn declared_tab_is_not_spawned() {
        // ns_window is never dereferenced for a declared (keep_alive=false) tab.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false);
        assert!(!r.is_spawned("t0"));
        // It still shows up in the chrome DTOs.
        let dtos = r.tab_dtos();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "t0");
        // A declared tab is cold: the live-dot flag is false until it spawns.
        assert!(!dtos[0].spawned, "declared tab must report spawned = false");
    }

    #[test]
    fn missing_dir_sets_warn_flag() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/no/such/dir/xyz"), false);
        assert!(r.tab_dtos()[0].warn, "missing dir must set warn");
    }

    #[test]
    fn remove_drops_declared_entry() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false);
        r.add(&spec("t1", "/tmp"), false);
        r.remove("t0");
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t1".to_string()]);
    }

    #[test]
    fn reorder_reorders_declared_entries() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp"), false);
        r.add(&spec("b", "/tmp"), false);
        r.reorder(&["b".to_string(), "a".to_string()]);
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn unload_of_cold_tab_is_noop() {
        // A declared (never-spawned) tab has no surface to kill: unload reports no
        // active change and the tab stays put and cold.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false);
        assert_eq!(r.unload("t0"), None);
        assert!(!r.is_spawned("t0"));
        assert_eq!(r.tab_dtos().len(), 1);
    }

    #[test]
    fn unload_of_unknown_tab_is_noop() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false);
        assert_eq!(r.unload("nope"), None);
        assert_eq!(r.tab_dtos().len(), 1);
    }

    #[test]
    fn neighbor_prefers_next_then_previous() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp"), false);
        r.add(&spec("b", "/tmp"), false);
        r.add(&spec("c", "/tmp"), false);
        // Middle tab → the next one.
        assert_eq!(r.neighbor_id(1).as_deref(), Some("c"));
        // Last tab → fall back to the previous one.
        assert_eq!(r.neighbor_id(2).as_deref(), Some("b"));
        // First tab → the next one.
        assert_eq!(r.neighbor_id(0).as_deref(), Some("b"));
    }

    #[test]
    fn neighbor_of_lone_tab_is_none() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("only", "/tmp"), false);
        assert_eq!(r.neighbor_id(0), None);
    }
}
