use crate::surface::{ghostty::GhosttySurface, PixelRect, SurfaceError, TabSpec, TerminalSurface};
use std::os::raw::c_void;

/// One probe-enabled tab's work item: `(id, dir, title, probe_cmd)`. The probe
/// runner substitutes `{dir}`/`{title}` into `probe_cmd` and runs it with cwd = dir.
pub type ProbeTarget = (String, std::path::PathBuf, String, String);

/// Display descriptor sent to the web chrome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TabDto {
    pub id: String,
    pub title: String,
    pub warn: bool,            // dir missing at materialize time
    pub spawned: bool,         // surface is live (load_on_open or already focused) vs cold/declared
    pub group: Option<String>, // [[window.group]] membership; None = loose (headerless)
    pub has_probe: bool,       // a session-probe command is configured for this tab
    pub has_kill: bool,        // a session-kill command is configured for this tab
    pub has_cmd: bool,         // a startup command is configured (→ presence dot can offer restart)
    pub tree: bool,            // row belongs to a project-tree (root) section
    #[serde(rename = "treePath")]
    pub tree_path: Vec<String>, // folder segments between root and project
    /// The tab's live surface has been moved to a popped-out window's registry
    /// (`Registry::detach`) — it is present here as a placeholder only, has no
    /// local surface, and `spawned` is always false alongside this.
    pub detached: bool,
    /// Last-known session presence: `"on"` (live), `"ghost"` (crashed but restorable), `"off"`
    /// (confirmed absent), or `null` — the manager's `PresenceCache` (see manager.rs) has no
    /// record for this tab yet. `null` covers two different cases and the chrome renders them
    /// differently: `toComponentDto` (ui/index.html) passes `null` straight through only when
    /// `has_probe` is false (no probe configured, permanently), and chrome-core renders **no
    /// dot at all** for that; for a `has_probe: true` tab it instead floors `null` to `"off"`,
    /// so an unprobed tab paints a hollow ring — visually identical to a confirmed-absent
    /// session — until the first probe result overwrites it. The Registry never sets this
    /// field — it's `None` here and patched by the manager from the cache so a (re)opened
    /// window paints its dots on the first render instead of waiting for a post-boot probe
    /// emit.
    pub presence: Option<crate::probe::Presence>,
}

/// A tab's surface is live, cold, or detached. Cold = not yet spawned, or
/// unloaded — retains its `TabSpec` on `TabEntry` so it can (re)spawn locally.
/// The `TabSpec` lives on `TabEntry`, not in the slot, so a cold tab always retains
/// what it needs to (re)spawn — `unload` returns a live tab to `Cold` without
/// losing its spec.
enum TabSlot {
    Spawned(GhosttySurface),
    Cold,
    /// The tab's live surface has been moved OUT to a popped-out window's own
    /// `Registry` (via `detach`), without freeing the PTY. This slot holds no
    /// surface — it's a placeholder that keeps the tab's identity present so
    /// hot-reload `reconcile` counts it as still here (not missing → no
    /// respawn; not stale → no duplicate add). `attach` is the return path,
    /// putting a surface back as `Spawned` when the tab moves home. Every
    /// local (this-registry) operation on a `Detached` tab is a no-op: there
    /// is nothing here to spawn, show, kill, or free — the surface and its
    /// lifecycle belong to whichever registry currently holds it `Spawned`.
    Detached,
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
    ///
    /// A failed *eager* spawn is non-fatal: the tab is still added as a **cold**
    /// entry (it shows in the sidebar and retries on next `activate`/focus) and
    /// the `SurfaceError` is returned for the caller to surface — never a panic,
    /// since one bad surface must not take down the window. A declared tab can't
    /// fail here (no spawn attempted) → always `Ok`.
    pub fn add(&mut self, spec: &TabSpec, load_on_open: bool) -> Result<(), SurfaceError> {
        let warn = !spec.dir.exists();
        let mut err = None;
        let slot = if load_on_open {
            match GhosttySurface::new(self.ns_window, self.last_rect, spec) {
                Ok(s) => {
                    s.hide();
                    TabSlot::Spawned(s)
                }
                Err(e) => {
                    err = Some(e);
                    TabSlot::Cold
                }
            }
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
        err.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    pub fn is_spawned(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && matches!(t.slot, TabSlot::Spawned(_)))
    }

    /// Is tab `id` currently a `Detached` placeholder (its live surface popped
    /// out into another window's registry)? The hot-reload reconcile
    /// (`manager::apply_tab_reconcile`) consults this to leave a popped-out tab's
    /// placeholder untouched — a config diff must never remove/respawn a tab
    /// whose surface it doesn't hold, or the live PTY is clobbered and `redock`
    /// loses its return slot. `id` is the same `Tab::key` (`id`-else-normalized-
    /// `dir`) the registry keys every entry on.
    pub fn is_detached(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && matches!(t.slot, TabSlot::Detached))
    }

    /// Does tab `id` have a `probe` command (⇒ a presence dot the scheduler tracks)? `activate_tab`
    /// consults this so it only arms a session-start await when cold-spawning a tab that actually
    /// has a session to come up — a probe-less tab has no dot, so awaiting one would just burst to
    /// CAP for nothing. Unknown id ⇒ `false`.
    pub fn tab_has_probe(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && t.spec.probe.is_some())
    }

    /// Test-only: force tab `id`'s slot straight to `Detached`, bypassing
    /// `detach` (which needs a real `Spawned` surface to extract — unavailable
    /// in a unit test with no AppKit). `TabSlot::Detached` carries no data, so
    /// this is a safe, direct state construction — it exercises every
    /// Detached-tab code path (`describe`, `unload`, `start_session`, `remove`,
    /// `close_all`, a second `detach`) without ever touching a `GhosttySurface`.
    #[cfg(test)]
    pub fn force_detached(&mut self, id: &str) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.slot = TabSlot::Detached;
        }
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
                has_probe: t.spec.probe.is_some(),
                has_kill: t.spec.kill.is_some(),
                has_cmd: t.spec.startup.is_some(),
                tree: t.spec.tree,
                tree_path: t.spec.tree_path.clone(),
                detached: matches!(t.slot, TabSlot::Detached),
                presence: None, // filled by the manager's PresenceCache, not the Registry
            })
            .collect()
    }

    /// `(id, dir, title, probe_cmd)` for every tab with a configured probe — the
    /// work-list the probe runner snapshots. Includes cold tabs (a session can
    /// exist while warden's own surface is unloaded — that's the point).
    pub fn probe_targets(&self) -> Vec<ProbeTarget> {
        self.tabs
            .iter()
            .filter_map(|t| {
                t.spec
                    .probe
                    .as_ref()
                    .map(|p| (t.id.clone(), t.spec.dir.clone(), t.title.clone(), p.clone()))
            })
            .collect()
    }

    /// `(dir, title, kill_cmd)` for tab `id` if it has a configured kill command,
    /// else `None` (unknown tab, or no kill set). The caller substitutes
    /// `{dir}`/`{title}` into `kill_cmd` and runs it with cwd = dir. Independent of
    /// the surface being live — a session can exist while warden's surface is cold.
    pub fn kill_target(&self, id: &str) -> Option<(std::path::PathBuf, String, String)> {
        let t = self.tabs.iter().find(|t| t.id == id)?;
        t.spec
            .kill
            .as_ref()
            .map(|k| (t.spec.dir.clone(), t.title.clone(), k.clone()))
    }

    /// Restart tab `id`'s session by typing its startup command into the **live** shell and
    /// submitting it (`TerminalSurface::run_command` — text inject + a real Enter keypress) — the
    /// runtime twin of how the tab launched. Preserves the terminal/scrollback (no respawn). Returns
    /// `false` (no-op) when the tab is unknown, has no startup command, or is cold — a cold tab has
    /// no shell to type into and is (re)started by activating it. The chrome only offers this on a
    /// live+startable tab with an absent session, so `false` means a stale/racing click.
    pub fn start_session(&self, id: &str) -> bool {
        let Some(t) = self.tabs.iter().find(|t| t.id == id) else {
            return false;
        };
        let Some(cmd) = t.spec.startup.as_ref() else {
            return false;
        };
        match &t.slot {
            TabSlot::Spawned(s) => {
                s.run_command(cmd);
                true
            }
            TabSlot::Cold => false,
            TabSlot::Detached => false, // no local shell — its surface lives elsewhere
        }
    }

    /// Apply a kept tab's in-place metadata (title/group/probe/kill) from a hot-reload —
    /// presentation + externally-run commands only, NEVER the surface/PTY. A config
    /// edit to title/group/probe/kill on a kept tab takes effect live without respawn:
    /// the row relabels, sidebar re-sections for group, new probe/kill picked up on
    /// the next poll/kill. No-op if `id` is unknown.
    pub fn set_meta(
        &mut self,
        id: &str,
        meta: &warden_config::TabMeta,
        tree: bool,
        tree_path: Vec<String>,
    ) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.title = meta.title.clone();
            t.spec.title = meta.title.clone();
            t.spec.group = meta.group.clone();
            t.spec.probe = meta.probe.clone();
            t.spec.kill = meta.kill.clone();
            // Tree-ness can flip on a kept tab when its group moves between a root
            // section and a plain group (curated↔discovered shadowing) — recomputed
            // upstream in `reconcile_ops`, so it must be applied here too, not left
            // stale from the prior render.
            t.spec.tree = tree;
            t.spec.tree_path = tree_path;
        }
    }

    /// The display title of tab `id` (for a detached window's banner), or `None` if unknown.
    pub fn tab_title(&self, id: &str) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
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
    /// A spawn failure leaves the tab cold and returns the error (the caller
    /// surfaces it); the tab can be retried by activating it again. Never panics.
    /// Spawn tab `idx`'s surface if it is `Cold`. Returns `true` iff it spawned **this call** (the
    /// tab was cold) — the signal `activate` forwards so a caller can arm a session-start await only
    /// when a fresh surface actually ran its `initial_input` (an already-live tab starts nothing).
    fn ensure_spawned(&mut self, idx: usize) -> Result<bool, SurfaceError> {
        if let TabSlot::Cold = self.tabs[idx].slot {
            let s = GhosttySurface::new(self.ns_window, self.last_rect, &self.tabs[idx].spec)?;
            self.tabs[idx].slot = TabSlot::Spawned(s);
            return Ok(true);
        }
        Ok(false)
    }

    /// Kill tab `id`'s surface + PTY, returning it to cold (it respawns a fresh
    /// shell on next focus, exactly like a never-opened tab). No-op if the tab is
    /// unknown or already cold. If the killed tab was the active one, switch to an
    /// already-**live** neighbour so unloading never spawns a fresh surface just to
    /// fill the hole (see `shell_core::pick_live_neighbour`); return that neighbour's
    /// id for the chrome to move its highlight to. `None` if nothing live remains —
    /// the chrome then blanks the hole rather than waking a cold tab.
    pub fn unload(&mut self, id: &str) -> Option<String> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        match std::mem::replace(&mut self.tabs[idx].slot, TabSlot::Cold) {
            TabSlot::Spawned(s) => s.close(),
            TabSlot::Cold => return None, // nothing live to kill
            TabSlot::Detached => {
                // No local surface to kill — it lives in another window's
                // registry (a pop-out). Restore the placeholder; leaving it
                // `Cold` here would make it look locally respawnable, which
                // would spawn a second surface for the same tab identity.
                self.tabs[idx].slot = TabSlot::Detached;
                return None;
            }
        }
        if self.active.as_deref() == Some(id) {
            self.active = None;
            let live: Vec<bool> = self
                .tabs
                .iter()
                .map(|t| matches!(t.slot, TabSlot::Spawned(_)))
                .collect();
            if let Some(n) = shell_core::pick_live_neighbour(idx, &live) {
                let next = self.tabs[n].id.clone();
                // The neighbour is already live (pick_live_neighbour only returns
                // spawned tabs), so this activate never spawns and can't fail.
                let _ = self.activate(&next);
                return Some(next);
            }
        }
        None
    }

    /// Detach tab `id`'s live surface for a move to another window's `Registry`
    /// (pop-out): extracts the `Spawned` surface and leaves the slot `Detached`
    /// — a placeholder that keeps the tab present for `reconcile` (not missing
    /// → no respawn; not stale → no duplicate) while the actual `GhosttySurface`
    /// moves to the caller (Task 11's manager code reparents its AppKit view and
    /// calls `attach` on the destination registry). The surface is **returned,
    /// never closed** — closing it here would kill the PTY the whole feature
    /// exists to preserve.
    ///
    /// `None` for: an unknown id; a `Cold` tab (nothing live to move — restored
    /// to `Cold`, not left `Detached`, since there is no surface anywhere to
    /// stand the placeholder in for); or a tab already `Detached` (restored to
    /// `Detached` — already gone elsewhere, calling this twice is a no-op, not
    /// a second extraction).
    pub fn detach(&mut self, id: &str) -> Option<GhosttySurface> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        match std::mem::replace(&mut self.tabs[idx].slot, TabSlot::Detached) {
            TabSlot::Spawned(s) => Some(s),
            TabSlot::Cold => {
                self.tabs[idx].slot = TabSlot::Cold;
                None
            }
            TabSlot::Detached => {
                self.tabs[idx].slot = TabSlot::Detached;
                None
            }
        }
    }

    /// Bring tab `id` live in place if it is `Cold`, spawning its surface against this
    /// window — so a never-opened tab can be popped out (pop-out then `detach`s the now-live
    /// surface and reparents it into its own window). No-op if the tab is already `Spawned`
    /// or `Detached` (a detached tab's live surface is in another window — never respawn a
    /// second one), and a no-op returning `Ok` if `id` is unknown (the caller's follow-up
    /// `detach` surfaces the not-found). Errors only if the spawn itself fails.
    pub fn ensure_spawned_by_id(&mut self, id: &str) -> Result<(), SurfaceError> {
        if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
            self.ensure_spawned(idx)?;
        }
        Ok(())
    }

    /// Return a surface to slot `id` as `Spawned` — the inverse of `detach`,
    /// called when a popped-out window's tab is reparented back into its
    /// origin registry. Succeeds from a `Detached` **or** a `Cold` slot →
    /// `Ok(())`, and the slot becomes `Spawned(surface)`.
    ///
    /// Both source slots are real return paths, which is why this accepts
    /// either: if the origin window stayed open while the tab was popped out,
    /// its slot is still the `Detached` placeholder `detach` left. If the user
    /// *closed* the origin window first, redock reopens it from config — which
    /// rebuilds that tab as a fresh `Cold` (or a freshly-spawned surface the
    /// caller then `unload`s back to `Cold`) — so the slot the returning
    /// surface lands in is `Cold`. Either way the reparented surface takes the
    /// slot.
    ///
    /// On failure — unknown `id`, or a slot that is already `Spawned` (a
    /// live surface already occupies it; overwriting would leak that surface's
    /// PTY) — the surface is handed **back** as `Err(surface)` rather than
    /// silently dropped. A `bool` return (as the task brief's other suggested
    /// shape) was rejected for this reason: on the failure path the `surface`
    /// parameter would simply fall out of scope and hit `GhosttySurface`'s
    /// `Drop` safety net, closing a surface the caller doesn't expect to lose.
    /// Returning it keeps that decision with the caller instead of making it
    /// silently here.
    pub fn attach(&mut self, id: &str, surface: GhosttySurface) -> Result<(), GhosttySurface> {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return Err(surface);
        };
        match t.slot {
            TabSlot::Cold | TabSlot::Detached => {
                t.slot = TabSlot::Spawned(surface);
                Ok(())
            }
            TabSlot::Spawned(_) => Err(surface),
        }
    }

    /// Show + focus the tab `id` (spawning it first if declared); hide all others.
    ///
    /// If the lazy spawn fails the tab is marked active anyway but stays cold, so
    /// the hole shows the blank placeholder (no live surface for `idx`); the error
    /// is returned for the caller to surface, and re-activating retries. Returns
    /// `Ok` for an unknown id (no-op) and for an already-spawned tab.
    /// Show tab `id` (spawning it if cold) and hide the rest. Returns `Ok(true)` iff this call
    /// spawned a **fresh** surface (the tab was cold) — so the caller can arm a session-start await
    /// only when `initial_input` actually ran; `Ok(false)` for a warm tab-switch. An unknown id is a
    /// no-op `Ok(false)`.
    pub fn activate(&mut self, id: &str) -> Result<bool, SurfaceError> {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return Ok(false);
        };
        let spawned = self.ensure_spawned(idx);
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
        spawned
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
            group: None,
            probe: None,
            kill: None,
            tree: false,
            tree_path: Vec::new(),
        }
    }
    fn spec_with_probe(id: &str, dir: &str, probe: Option<&str>) -> TabSpec {
        TabSpec {
            id: id.to_string(),
            title: id.to_string(),
            dir: PathBuf::from(dir),
            shell: "sh".to_string(),
            startup: None,
            group: None,
            probe: probe.map(String::from),
            kill: None,
            tree: false,
            tree_path: Vec::new(),
        }
    }

    #[test]
    fn declared_tab_is_not_spawned() {
        // ns_window is never dereferenced for a declared (load_on_open=false) tab.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
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
        let _ = r.add(&spec("t0", "/no/such/dir/xyz"), false);
        assert!(r.tab_dtos()[0].warn, "missing dir must set warn");
    }

    #[test]
    fn remove_drops_declared_entry() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        let _ = r.add(&spec("t1", "/tmp"), false);
        r.remove("t0");
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t1".to_string()]);
    }

    #[test]
    fn reorder_reorders_declared_entries() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("a", "/tmp"), false);
        let _ = r.add(&spec("b", "/tmp"), false);
        r.reorder(&["b".to_string(), "a".to_string()]);
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn set_meta_updates_spec_without_touching_surface() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        assert_eq!(r.tab_dtos()[0].group, None);
        assert!(!r.tab_dtos()[0].has_probe && !r.tab_dtos()[0].has_kill);
        r.set_meta(
            "t0",
            &warden_config::TabMeta {
                title: "Renamed".to_string(),
                group: Some("backend".into()),
                probe: Some("probe-x".into()),
                kill: Some("kill-y".into()),
            },
            true,
            vec!["gh".into(), "lockyc".into()],
        );
        let d = &r.tab_dtos()[0];
        assert_eq!(d.title, "Renamed", "title relabeled in place");
        assert_eq!(d.group.as_deref(), Some("backend"));
        assert!(d.has_probe, "probe applied in place");
        assert!(d.has_kill, "kill applied in place");
        assert!(d.tree, "tree-ness applied in place");
        assert_eq!(d.tree_path, vec!["gh".to_string(), "lockyc".to_string()]);
        // No respawn — a cold tab stays cold.
        assert!(!r.is_spawned("t0"));
        // Unknown id is a no-op (doesn't panic).
        r.set_meta(
            "nope",
            &warden_config::TabMeta {
                title: "x".to_string(),
                group: None,
                probe: None,
                kill: None,
            },
            false,
            Vec::new(),
        );
    }

    #[test]
    fn unload_of_cold_tab_is_noop() {
        // A declared (never-spawned) tab has no surface to kill: unload reports no
        // active change and the tab stays put and cold.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        assert_eq!(r.unload("t0"), None);
        assert!(!r.is_spawned("t0"));
        assert_eq!(r.tab_dtos().len(), 1);
    }

    #[test]
    fn unload_of_unknown_tab_is_noop() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        assert_eq!(r.unload("nope"), None);
        assert_eq!(r.tab_dtos().len(), 1);
    }

    // --- detach / attach (Task 10: pop a tab out) ---------------------------
    //
    // A real `Spawned` slot needs a live `GhosttySurface`, which needs AppKit —
    // unconstructable in this unit-test process (ns_window is null; every test
    // in this file stays on load_on_open=false for exactly this reason). So the
    // slot-state transitions below use `force_detached` (a safe, data-free
    // direct construction of `TabSlot::Detached`) to reach the states a real
    // pop-out would produce, and prove `detach`/`unload`/`start_session`/
    // `remove`/`close_all`/a second `detach` all treat that slot correctly
    // without ever touching a `GhosttySurface`. The one thing genuinely
    // untestable here is `detach` extracting a *live* surface and `attach`
    // accepting one back — that needs a real surface, so it's covered by a
    // GUI-driven check instead (see Task 12 / the manual verification note in
    // the task report).

    #[test]
    fn detach_of_cold_tab_returns_none_and_stays_cold() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        assert!(r.detach("t0").is_none());
        assert!(!r.is_spawned("t0"));
        assert!(!r.is_detached("t0"), "a cold tab must not become Detached");
        assert!(!r.tab_dtos()[0].detached);
    }

    #[test]
    fn detach_of_unknown_tab_returns_none() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        assert!(r.detach("nope").is_none());
    }

    #[test]
    fn detach_of_already_detached_tab_is_noop() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.force_detached("t0");
        assert!(
            r.detach("t0").is_none(),
            "nothing live here to extract a second time"
        );
        assert!(r.is_detached("t0"), "must stay Detached, not reset to Cold");
    }

    #[test]
    fn detached_tab_is_still_listed_by_describe() {
        // reconcile-relevant: the placeholder keeps the tab present (not
        // missing → no respawn attempt; not stale → no duplicate add).
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        let _ = r.add(&spec("t1", "/tmp"), false);
        r.force_detached("t0");
        let dtos = r.tab_dtos();
        assert_eq!(
            dtos.len(),
            2,
            "detached tab must still be listed, not dropped"
        );
        let t0 = dtos.iter().find(|d| d.id == "t0").unwrap();
        assert!(t0.detached, "detached: true");
        assert!(!t0.spawned, "spawned: false alongside detached: true");
    }

    // `attach` itself is NOT unit-tested: every path through it needs a real
    // `GhosttySurface` argument (there is no valid empty/placeholder value to
    // construct one from — see the module-level note above), so both its
    // success arms (Cold -> Spawned, when redock reopened the origin, and
    // Detached -> Spawned, when the origin stayed open) and its Err-returns-the-
    // surface failure arm (a `Spawned` slot) need a live surface to even call it
    // with. The slot gate it relies on is exercised structurally above via
    // `force_detached` + `is_detached`. See the task report for the GUI-driven
    // verification plan.

    #[test]
    fn unload_of_detached_tab_is_noop_and_stays_detached() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.force_detached("t0");
        assert_eq!(r.unload("t0"), None, "no local surface to kill");
        assert!(
            r.is_detached("t0"),
            "must stay Detached, not fall back to Cold"
        );
    }

    #[test]
    fn ensure_spawned_by_id_never_respawns_a_detached_tab_or_panics_on_unknown() {
        // Pop-out calls this to bring a COLD tab live before detaching it. It must NOT touch a
        // Detached tab (whose live surface is in another window — a second spawn would duplicate
        // it), and an unknown id must be a clean no-op (the caller's follow-up `detach` reports
        // not-found). The Cold→spawn path needs AppKit and is GUI-only (Task-12 driver).
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.force_detached("t0");
        assert!(r.ensure_spawned_by_id("t0").is_ok());
        assert!(r.is_detached("t0"), "must stay Detached, not respawn");
        assert!(!r.is_spawned("t0"), "no second surface spawned here");
        assert!(
            r.ensure_spawned_by_id("does-not-exist").is_ok(),
            "unknown id is a clean no-op"
        );
    }

    #[test]
    fn start_session_noop_for_detached_tab() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut with_cmd = spec("t0", "/tmp");
        with_cmd.startup = Some("amux".into());
        let _ = r.add(&with_cmd, false);
        r.force_detached("t0");
        assert!(
            !r.start_session("t0"),
            "no local shell to type into once detached"
        );
    }

    #[test]
    fn remove_of_detached_tab_does_not_panic_and_drops_placeholder() {
        // Must not attempt to free anything (there is no surface here to
        // free — it lives in the popped-out window's registry).
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        let _ = r.add(&spec("t1", "/tmp"), false);
        r.force_detached("t0");
        r.remove("t0");
        let ids: Vec<_> = r.tab_dtos().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["t1".to_string()]);
    }

    #[test]
    fn close_all_with_detached_tab_does_not_panic() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.force_detached("t0");
        r.close_all();
        assert_eq!(r.tab_dtos().len(), 0);
    }

    #[test]
    fn has_probe_flag_reflects_spec() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec_with_probe("t0", "/tmp", Some("x")), false);
        let _ = r.add(&spec_with_probe("t1", "/tmp", None), false);
        let dtos = r.tab_dtos();
        assert!(dtos[0].has_probe);
        assert!(!dtos[1].has_probe);
    }

    #[test]
    fn probe_targets_lists_only_probe_enabled_tabs() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec_with_probe("t0", "/tmp/a", Some("cmd-a")), false);
        let _ = r.add(&spec_with_probe("t1", "/tmp/b", None), false);
        let targets = r.probe_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "t0");
        assert_eq!(targets[0].1, PathBuf::from("/tmp/a"));
        assert_eq!(targets[0].2, "t0"); // title (= id here)
        assert_eq!(targets[0].3, "cmd-a");
    }

    #[test]
    fn has_kill_flag_reflects_spec() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s_with = spec("t0", "/tmp");
        s_with.kill = Some("kill-cmd {dir}".into());
        let _ = r.add(&s_with, false);
        let _ = r.add(&spec("t1", "/tmp"), false); // kill: None
        let dtos = r.tab_dtos();
        assert!(dtos[0].has_kill);
        assert!(!dtos[1].has_kill);
    }

    #[test]
    fn kill_target_returns_dir_title_cmd_only_when_set() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s = spec("t0", "/tmp/a");
        s.kill = Some("kill {title}".into());
        let _ = r.add(&s, false);
        let _ = r.add(&spec("t1", "/tmp/b"), false); // no kill
        assert_eq!(
            r.kill_target("t0"),
            Some((
                std::path::PathBuf::from("/tmp/a"),
                "t0".to_string(),
                "kill {title}".to_string()
            ))
        );
        assert_eq!(r.kill_target("t1"), None); // no kill command
        assert_eq!(r.kill_target("nope"), None); // unknown id
    }

    #[test]
    fn has_cmd_flag_reflects_startup() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s_with = spec("t0", "/tmp");
        s_with.startup = Some("amux".into());
        let _ = r.add(&s_with, false);
        let _ = r.add(&spec("t1", "/tmp"), false); // startup: None
        let dtos = r.tab_dtos();
        assert!(dtos[0].has_cmd);
        assert!(!dtos[1].has_cmd);
    }

    #[test]
    fn respawn_sequence_replaces_spec_for_same_id() {
        // `manager.rs`'s `WindowOp::Update` respawn step is exactly this sequence —
        // `remove(id)` then `add(&new_spec, ..)` — applied to a kept tab whose terminal
        // spec (e.g. `dir`) changed but whose id (identity) is unchanged. Prove it swaps
        // the underlying spec in place, with no stray duplicate entry left behind. This
        // is the smallest real unit that proves the respawn mechanic without an
        // AppHandle/WindowManager::apply (which needs a live Tauri app).
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec_with_probe("t0", "/tmp/old", Some("probe-cmd")), false);
        let _ = r.add(
            &spec_with_probe("t1", "/tmp/other", Some("probe-cmd")),
            false,
        );

        r.remove("t0");
        let _ = r.add(&spec_with_probe("t0", "/tmp/new", Some("probe-cmd")), false);

        let dtos = r.tab_dtos();
        assert_eq!(
            dtos.len(),
            2,
            "respawn must not leave a stray duplicate entry"
        );

        let targets = r.probe_targets();
        let t0_dir = targets.iter().find(|t| t.0 == "t0").unwrap().1.clone();
        assert_eq!(
            t0_dir,
            PathBuf::from("/tmp/new"),
            "respawn must replace the spec (new dir), not keep the stale one"
        );
        assert!(
            !r.is_spawned("t0"),
            "load_on_open=false respawn stays cold until next activate"
        );
    }

    #[test]
    fn start_session_noop_when_cold_no_cmd_or_unknown() {
        // Declared tabs are cold (no ns_window deref). start_session sends nothing and reports false
        // for: a cold tab (even with a startup cmd), a tab without a startup cmd, and an unknown id.
        // The live-send path needs a real surface, so it isn't exercised here (same as focus/spawn).
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut with_cmd = spec("t0", "/tmp");
        with_cmd.startup = Some("amux".into());
        let _ = r.add(&with_cmd, false); // cold, has cmd
        let _ = r.add(&spec("t1", "/tmp"), false); // cold, no cmd
        assert!(!r.start_session("t0"), "cold tab: nothing to type into");
        assert!(
            !r.start_session("t1"),
            "no startup command → nothing to send"
        );
        assert!(!r.start_session("nope"), "unknown id");
    }
}
