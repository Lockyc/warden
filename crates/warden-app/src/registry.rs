use crate::surface::{ghostty::GhosttySurface, PixelRect, TabSpec, TerminalSurface};
use std::os::raw::c_void;

/// Display descriptor sent to the web chrome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TabDto {
    pub id: String,
    pub title: String,
    pub warn: bool,            // dir missing at materialize time
    pub spawned: bool,         // surface is live (load_on_open or already focused) vs cold/declared
    pub group: Option<String>, // [[window.group]] membership; None = loose (headerless)
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

    /// Add a tab. `load_on_open=true` spawns now (eager); `false` declares it
    /// (lazy — spawns on first `activate`). [spec §3]
    pub fn add(&mut self, spec: &TabSpec, load_on_open: bool) {
        let warn = !spec.dir.exists();
        let slot = if load_on_open {
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
                group: t.spec.group.clone(),
            })
            .collect()
    }

    /// Reassign a tab's group (presentation only — does not touch its surface/PTY).
    /// Used by a hot-reload `set_groups` so regrouping or a group rename re-sections
    /// the sidebar without killing the terminal. No-op if `id` is unknown.
    pub fn set_group(&mut self, id: &str, group: Option<String>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.spec.group = group;
        }
    }

    /// The id of the tab owning the spawned surface with handle `surface_id`
    /// (`GhosttySurface::id`), if any — routes a per-surface signal (bell /
    /// notification) back to its tab.
    pub fn tab_of_surface(&self, surface_id: usize) -> Option<&str> {
        self.tabs.iter().find_map(|t| match &t.slot {
            TabSlot::Spawned(s) if s.id() == surface_id => Some(t.id.as_str()),
            _ => None,
        })
    }

    /// The currently-active (on-screen) tab id, if any.
    pub fn active_tab(&self) -> Option<&str> {
        self.active.as_deref()
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

    /// Kill tab `id`'s surface + PTY, returning it to cold (it respawns a fresh
    /// shell on next focus, exactly like a never-opened tab). No-op if the tab is
    /// unknown or already cold. If the killed tab was the active one, switch to an
    /// already-**live** neighbour so unloading never spawns a fresh surface just to
    /// fill the hole (see `pick_live_neighbour`); return that neighbour's id for the
    /// chrome to move its highlight to. `None` if nothing live remains — the chrome
    /// then blanks the hole rather than waking a cold tab.
    pub fn unload(&mut self, id: &str) -> Option<String> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        match std::mem::replace(&mut self.tabs[idx].slot, TabSlot::Cold) {
            TabSlot::Spawned(s) => s.close(),
            TabSlot::Cold => return None, // nothing live to kill
        }
        if self.active.as_deref() == Some(id) {
            self.active = None;
            let live: Vec<bool> = self
                .tabs
                .iter()
                .map(|t| matches!(t.slot, TabSlot::Spawned(_)))
                .collect();
            if let Some(n) = pick_live_neighbour(idx, &live) {
                let next = self.tabs[n].id.clone();
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

/// Index of the tab to activate after the tab at `idx` is killed, given each tab's live
/// (spawned) state. Prefer the immediate next tab **if it is live** (natural forward motion),
/// else the nearest live tab to the left (the one you usually came from), else the nearest live
/// tab to the right. `None` ⇒ nothing live to show — the caller leaves the hole blank rather
/// than spawning a cold tab just to fill it. Pure index logic, so it's unit-testable without
/// real surfaces (which `add(.., true)` can't fabricate against a null `ns_window`).
fn pick_live_neighbour(idx: usize, live: &[bool]) -> Option<usize> {
    if live.get(idx + 1).copied().unwrap_or(false) {
        return Some(idx + 1);
    }
    if let Some(p) = (0..idx).rev().find(|&i| live[i]) {
        return Some(p);
    }
    ((idx + 1)..live.len()).find(|&i| live[i])
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
            group: None,
        }
    }

    #[test]
    fn declared_tab_is_not_spawned() {
        // ns_window is never dereferenced for a declared (load_on_open=false) tab.
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
    fn set_group_updates_dto_without_touching_surface() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false);
        assert_eq!(r.tab_dtos()[0].group, None);
        r.set_group("t0", Some("backend".into()));
        assert_eq!(r.tab_dtos()[0].group.as_deref(), Some("backend"));
        // Cold tab stays cold — regrouping never spawns.
        assert!(!r.is_spawned("t0"));
        // Unknown id is a no-op (doesn't panic).
        r.set_group("nope", Some("x".into()));
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
    fn pick_live_neighbour_prefers_next_when_live() {
        // killed@1; next@2 is live → take it (forward motion).
        assert_eq!(pick_live_neighbour(1, &[true, false, true, true]), Some(2));
    }

    #[test]
    fn pick_live_neighbour_falls_back_to_previous_when_next_cold() {
        // killed@1; next@2 cold; previous@0 live → previous (don't wake the cold next).
        assert_eq!(pick_live_neighbour(1, &[true, false, false]), Some(0));
    }

    #[test]
    fn pick_live_neighbour_prefers_nearest_live_left_over_right() {
        // killed@2; next@3 cold; live to the left (@1) and far right (@4) → left wins.
        assert_eq!(
            pick_live_neighbour(2, &[false, true, false, false, true]),
            Some(1)
        );
    }

    #[test]
    fn pick_live_neighbour_uses_right_when_nothing_live_left() {
        // killed@0; next@1 cold; nothing to the left; live@3 → scan right to it.
        assert_eq!(
            pick_live_neighbour(0, &[false, false, false, true]),
            Some(3)
        );
    }

    #[test]
    fn pick_live_neighbour_none_when_nothing_live() {
        // No live tab anywhere → blank the hole, never spawn one to fill it.
        assert_eq!(pick_live_neighbour(1, &[false, false, false]), None);
    }
}
