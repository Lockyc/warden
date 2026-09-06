use crate::surface::{ghostty::GhosttySurface, PixelRect, SurfaceError, TabSpec, TerminalSurface};
use std::os::raw::c_void;

/// One probe-enabled tab's work item: `(id, dir, title, probe_cmd)`. The probe
/// runner substitutes `{dir}`/`{title}` into `probe_cmd` and runs it with cwd = dir.
pub type ProbeTarget = (String, std::path::PathBuf, String, String);

/// The chrome-facing shape of a declared split — the two layout facts the chrome seeds the
/// divider from. `side` is the wire string the chrome's `.secondary-left` class keys on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SplitLayoutDto {
    pub side: &'static str,
    pub size: f64,
}

/// Display descriptor sent to the web chrome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TabDto {
    pub id: String,
    pub title: String,
    pub warn: bool,            // dir missing NOW — derived per render (see `tab_dtos`)
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
    /// Whether this tab's SECONDARY pane is live — `false` both when the tab is unsplit (no
    /// secondary exists) and when a split's secondary is cold. `spawned` above is primary-only
    /// (see `tab_dtos`), so the chrome needs this alongside it to decide the second hole's own
    /// backstop: a live primary beside a cold secondary must still cover the secondary's hole
    /// with `#empty-state-2`, or it leaks the desktop through the transparent window.
    pub secondary_spawned: bool,
    /// Whether this tab STRUCTURALLY has a second pane — distinct from `secondary_spawned`
    /// above, which is that pane's LIVENESS. Both are needed, and forwarding only the second
    /// is what let the chrome and the registry disagree: a hot-reload `respawn_tabs` rebuilds
    /// a split tab via `remove` + `add`, i.e. **unsplit** for a runtime (⌘D) split — only a
    /// config-declared split survives a rebuild, because `add` re-declares it from
    /// `TabSpec.split` — and the `warden:refresh` that follows carried no
    /// structural answer at all — so the chrome kept its `splitById` `true` and went on
    /// rendering a divider and a permanently empty second pane over a tab that no longer had
    /// one, self-healing only on the next `onSelect` for that tab, which never comes if that
    /// tab stays selected. Carrying it on every snapshot makes `activate_tab`'s `res.split` a
    /// fast path rather than the sole reconciler.
    pub split: bool,
    /// The declared (config) split's layout, or `None` for a tab with no config split — a
    /// runtime ⌘D split reports `None` here and `split: true` above. The chrome keys its
    /// "config split vs runtime split" behaviour (where the ratio is remembered, whether ✕
    /// is permanent) on this field's presence.
    pub split_layout: Option<SplitLayoutDto>,
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

/// Which of a tab's (at most two) panes an operation means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneIdx {
    Primary,
    Secondary,
}

impl PaneIdx {
    /// The wire index the chrome and `set_hole_rect` use: 0 = primary, 1 = secondary. The one
    /// home for that mapping in both directions — every command and event that carries a pane
    /// number goes through these two.
    pub fn index(self) -> usize {
        match self {
            PaneIdx::Primary => 0,
            PaneIdx::Secondary => 1,
        }
    }
    /// Inverse of [`index`](Self::index). Anything that isn't `1` is the primary rather than an
    /// error: a bad index must never leave a hole unpositioned or a click unrouted.
    pub fn from_index(i: usize) -> Self {
        if i == 1 {
            PaneIdx::Secondary
        } else {
            PaneIdx::Primary
        }
    }
}

/// One terminal within a tab: its spawn recipe plus its live/cold/detached slot.
struct Pane {
    spec: TabSpec,
    slot: TabSlot,
}

/// The spec a tab's SECOND pane runs: the tab's own shell, in the tab's own dir, with the
/// config split's `cmd` as its startup (a runtime split has none — the frame's scratch
/// terminal), under the distinct surface id `"<tab>::2"` that surface→tab routing
/// (`locate_surface`) and the surface event sink key on.
///
/// A free function with two callers — `split` (the ⌘D path) and `attach` (which recreates the
/// slot when a popped-out split returns to an origin window that was closed and rebuilt from
/// config, and so has no secondary pane any more). Neither may re-derive it: two spellings of
/// one spec would drift the second pane's surface id, and a mismatched id routes its bell,
/// notification and child-exit to nothing.
fn secondary_spec(primary: &TabSpec, tab_id: &str) -> TabSpec {
    let mut spec = primary.clone();
    spec.id = format!("{tab_id}::2");
    // A config split names what the second pane runs; a runtime split is a bare scratch shell.
    spec.startup = primary.split.as_ref().and_then(|s| s.startup.clone());
    spec.split = None;
    spec
}

/// A tab owns a `primary` pane and, when split, a `secondary`.
///
/// `primary`/`Option<secondary>` rather than a `Vec` so the agreed TWO-PANE CAP is
/// unrepresentable-otherwise: a third pane cannot be constructed by accident, and every
/// method has an obvious primary path. Arbitrary nesting was considered and declined —
/// see docs/native-splits-direction.md. If a tree is ever wanted it REPLACES this type
/// rather than growing out of it.
struct TabEntry {
    id: String,
    title: String,
    primary: Pane,
    secondary: Option<Pane>,
    /// Which pane has keyboard focus. Always `Primary` when `secondary` is `None`.
    focused: PaneIdx,
}

impl TabEntry {
    fn pane(&self, which: PaneIdx) -> Option<&Pane> {
        match which {
            PaneIdx::Primary => Some(&self.primary),
            PaneIdx::Secondary => self.secondary.as_ref(),
        }
    }
    fn pane_mut(&mut self, which: PaneIdx) -> Option<&mut Pane> {
        match which {
            PaneIdx::Primary => Some(&mut self.primary),
            PaneIdx::Secondary => self.secondary.as_mut(),
        }
    }
    /// Every live pane, primary first. The iteration order is load-bearing for teardown:
    /// callers that free surfaces rely on it being stable.
    fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        std::iter::once(&mut self.primary).chain(self.secondary.iter_mut())
    }
    fn panes(&self) -> impl Iterator<Item = &Pane> {
        std::iter::once(&self.primary).chain(self.secondary.iter())
    }
    /// Every live pane paired with its own `PaneIdx`, primary first. `activate`'s
    /// show/focus loop iterates this to compare each pane against `focused` — see the
    /// carried-finding note on `Registry::activate` for why the pairing (not the
    /// resulting `.focus()` call) is what this file's null-window tests can pin.
    fn panes_indexed(&self) -> impl Iterator<Item = (PaneIdx, &Pane)> {
        std::iter::once((PaneIdx::Primary, &self.primary))
            .chain(self.secondary.as_ref().map(|p| (PaneIdx::Secondary, p)))
    }
}

pub struct Registry {
    ns_window: *mut c_void,
    tabs: Vec<TabEntry>,
    active: Option<String>,
    /// Last reported rect per pane. Two, not one, because a cold pane that spawns later must
    /// be born at ITS hole's size — using the other pane's rect makes a freshly-spawned
    /// secondary render at the primary's width for one frame.
    last_rect: PixelRect,
    last_rect_secondary: PixelRect,
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
            last_rect_secondary: initial_rect,
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
            primary: Pane {
                spec: spec.clone(),
                slot,
            },
            secondary: None,
            focused: PaneIdx::Primary,
        });
        // A config split is STRUCTURAL: declare the pane now so the tab reads as split from its
        // first snapshot, and spawn it alongside an eager primary so `load_on_open` brings both
        // panes up together (a lazy tab's `activate` already spawns every cold pane). This eager
        // path (`load_on_open` + a config split) is pinned by inspection only — the registry
        // tests run against a null NSWindow and never spawn — the same test-double gap `attach`'s
        // `Detached`-retire branch records.
        if spec.split.is_some() {
            let idx = self.tabs.len() - 1;
            self.split(&spec.id);
            if matches!(self.tabs[idx].primary.slot, TabSlot::Spawned(_)) {
                match self.ensure_spawned(idx, PaneIdx::Secondary) {
                    Ok(_) => {
                        if let Some(TabSlot::Spawned(s)) =
                            self.tabs[idx].secondary.as_ref().map(|p| &p.slot)
                        {
                            s.hide();
                        }
                    }
                    Err(e) => eprintln!(
                        "warden: secondary spawn failed for tab {:?}: {e}",
                        spec.title
                    ),
                }
            }
        }
        err.map_or(Ok(()), Err)
    }

    /// Whether tab `id` currently has a second pane. Unknown tab = false.
    pub fn is_split(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .is_some_and(|t| t.secondary.is_some())
    }

    /// Any-pane, not primary-only: a tab is "spawned" if either pane is live. This is
    /// the check a future tab-row live dot needs (tab-level, not per-pane) — a split
    /// tab with only its secondary live must still read as spawned.
    #[cfg(test)]
    pub fn is_spawned(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .is_some_and(|t| t.panes().any(|p| matches!(p.slot, TabSlot::Spawned(_))))
    }

    /// Give tab `id` a second pane: the tab's own shell, in the tab's own dir, with NO
    /// startup command — the frame's scratch terminal. Idempotent: splitting an already
    /// split tab is a no-op, which is what keeps the two-pane cap true even if the chrome
    /// double-fires. Unknown tab is a no-op.
    ///
    /// The pane is created **Cold**, not spawned: `activate` brings cold panes live, exactly
    /// as a lazily-added tab (`add(load_on_open = false)`) already works. Spawning here would
    /// make this method the only one in the file that must succeed against a live NSWindow,
    /// and would make it untestable — the registry tests run against a null window.
    /// The `split_pane` command (Task 7) calls `activate` right after, so the pane still comes
    /// up immediately for the user.
    ///
    /// The secondary's `TabSpec.id` is `"<tab>::2"`, distinct from the tab id — see
    /// `secondary_spec`, the one home for that derivation.
    pub fn split(&mut self, id: &str) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if t.secondary.is_some() {
            return;
        }
        let spec = secondary_spec(&t.primary.spec, id);
        t.secondary = Some(Pane {
            spec,
            slot: TabSlot::Cold,
        });
    }

    /// Drop tab `id`'s second pane, freeing its surface. Returns whether there was one.
    /// Focus falls back to the primary — leaving `focused` on a pane that no longer
    /// exists would send keystrokes nowhere.
    pub fn close_secondary(&mut self, id: &str) -> bool {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        // `close()`, never a bare `take()` that lets `Drop` free it: `GhosttySurface::drop`
        // treats being dropped without `close()` as a registry bug and says so in debug
        // builds, so a plain take made the divider ✕ — the ordinary way a split ends — print a
        // false bug report every time. Same `mem::replace` + `close()` shape as `unload`,
        // `remove` and `close_all`.
        let had = match t.secondary.take() {
            Some(mut pane) => {
                if let TabSlot::Spawned(s) = std::mem::replace(&mut pane.slot, TabSlot::Cold) {
                    s.close();
                }
                true
            }
            None => false,
        };
        if had {
            t.focused = PaneIdx::Primary;
        }
        had
    }

    /// Point keyboard focus at one of tab `id`'s panes. Returns false (and changes
    /// nothing) if that pane does not exist.
    pub fn focus_pane(&mut self, id: &str, which: PaneIdx) -> bool {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if t.pane(which).is_none() {
            return false;
        }
        t.focused = which;
        true
    }

    /// Which pane of tab `id` has focus. `None` for an unknown tab.
    pub fn focused_pane(&self, id: &str) -> Option<PaneIdx> {
        self.tabs.iter().find(|t| t.id == id).map(|t| t.focused)
    }

    #[cfg(test)]
    pub fn secondary_spec_for_test(&self, id: &str) -> Option<TabSpec> {
        self.tabs
            .iter()
            .find(|t| t.id == id)?
            .secondary
            .as_ref()
            .map(|p| p.spec.clone())
    }

    /// Is tab `id` currently a `Detached` placeholder (its live surface popped
    /// out into another window's registry)? The hot-reload reconcile
    /// (`manager::apply_tab_reconcile`) consults this to leave a popped-out tab's
    /// placeholder untouched — a config diff must never remove/respawn a tab
    /// whose surface it doesn't hold, or the live PTY is clobbered and `redock`
    /// loses its return slot. `id` is the same `Tab::key` (`id`-else-normalized-
    /// `dir`) the registry keys every entry on.
    ///
    /// Primary-only, not any-pane, and still correct now that pop-out carries both panes:
    /// `detach` refuses unless the PRIMARY is live and only then touches the secondary, so
    /// a tab is detached exactly when its primary slot says so. Reading any-pane instead
    /// would call a tab detached on the strength of a secondary alone, which no path
    /// produces.
    pub fn is_detached(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && matches!(t.primary.slot, TabSlot::Detached))
    }

    /// Does tab `id` have a `probe` command (⇒ a presence dot the scheduler tracks)? `activate_tab`
    /// consults this so it only arms a session-start await when cold-spawning a tab that actually
    /// has a session to come up — a probe-less tab has no dot, so awaiting one would just burst to
    /// CAP for nothing. Unknown id ⇒ `false`.
    pub fn tab_has_probe(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && t.primary.spec.probe.is_some())
    }

    /// Test-only: force tab `id`'s slot straight to `Detached`, bypassing
    /// `detach` (which needs a real `Spawned` surface to extract — unavailable
    /// in a unit test with no AppKit). `TabSlot::Detached` carries no data, so
    /// this is a safe, direct state construction — it exercises every
    /// Detached-tab code path (`describe`, `unload`, `start_session`, `remove`,
    /// `close_all`, a second `detach`) without ever touching a `GhosttySurface`.
    #[cfg(test)]
    pub fn force_detached(&mut self, id: &str) {
        self.force_detached_pane(id, PaneIdx::Primary);
    }

    /// Test-only: the per-pane form of `force_detached` — pop-out now moves both panes,
    /// so the secondary has its own `Detached` state to construct and assert against.
    #[cfg(test)]
    pub fn force_detached_pane(&mut self, id: &str, which: PaneIdx) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            if let Some(p) = t.pane_mut(which) {
                p.slot = TabSlot::Detached;
            }
        }
    }

    /// Test-only: is pane `which` of tab `id` a `Detached` placeholder? `false` for a
    /// missing tab or pane.
    #[cfg(test)]
    pub fn is_pane_detached(&self, id: &str, which: PaneIdx) -> bool {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.pane(which))
            .is_some_and(|p| matches!(p.slot, TabSlot::Detached))
    }

    /// Snapshot every tab for the chrome.
    ///
    /// `warn` is **derived here, per render** — deliberately not cached on the entry.
    /// A tab's directory can appear or vanish long after the tab is materialized (the
    /// case that produced this: a config edit declared a tab ~30s before the repo was
    /// cloned into place, and the ⚠ then stuck for the whole session, since `set_meta`
    /// relabels a kept tab without touching a cached flag). The filesystem is the one
    /// source of truth for "does this dir exist"; don't reintroduce a copy. The cost is
    /// one `stat` per tab, and this runs per sidebar render (window init / reconcile /
    /// activate), not per frame.
    pub fn tab_dtos(&self) -> Vec<TabDto> {
        self.tabs
            .iter()
            .map(|t| TabDto {
                id: t.id.clone(),
                title: t.title.clone(),
                warn: !t.primary.spec.dir.exists(),
                spawned: matches!(t.primary.slot, TabSlot::Spawned(_)),
                group: t.primary.spec.group.clone(),
                has_probe: t.primary.spec.probe.is_some(),
                has_kill: t.primary.spec.kill.is_some(),
                has_cmd: t.primary.spec.startup.is_some(),
                tree: t.primary.spec.tree,
                tree_path: t.primary.spec.tree_path.clone(),
                detached: matches!(t.primary.slot, TabSlot::Detached),
                // Read straight off the `TabEntry` already in hand rather than a per-tab lookup
                // by id (fix round 1, cheap fix #2): re-finding `t` in `self.tabs` for every row
                // would turn this map into an O(n²) pass over the whole tab list.
                secondary_spawned: t
                    .secondary
                    .as_ref()
                    .is_some_and(|p| matches!(p.slot, TabSlot::Spawned(_))),
                split: t.secondary.is_some(),
                split_layout: t.primary.spec.split.as_ref().map(|s| SplitLayoutDto {
                    side: match s.side {
                        warden_config::SplitSide::Left => "left",
                        warden_config::SplitSide::Right => "right",
                    },
                    size: s.size,
                }),
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
                t.primary.spec.probe.as_ref().map(|p| {
                    (
                        t.id.clone(),
                        t.primary.spec.dir.clone(),
                        t.title.clone(),
                        p.clone(),
                    )
                })
            })
            .collect()
    }

    /// `(dir, title, kill_cmd)` for tab `id` if it has a configured kill command,
    /// else `None` (unknown tab, or no kill set). The caller substitutes
    /// `{dir}`/`{title}` into `kill_cmd` and runs it with cwd = dir. Independent of
    /// the surface being live — a session can exist while warden's surface is cold.
    pub fn kill_target(&self, id: &str) -> Option<(std::path::PathBuf, String, String)> {
        let t = self.tabs.iter().find(|t| t.id == id)?;
        t.primary
            .spec
            .kill
            .as_ref()
            .map(|k| (t.primary.spec.dir.clone(), t.title.clone(), k.clone()))
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
        let Some(cmd) = t.primary.spec.startup.as_ref() else {
            return false;
        };
        match &t.primary.slot {
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
            t.primary.spec.title = meta.title.clone();
            t.primary.spec.group = meta.group.clone();
            t.primary.spec.probe = meta.probe.clone();
            t.primary.spec.kill = meta.kill.clone();
            t.primary.spec.split = meta.split.clone();
            // Tree-ness can flip on a kept tab when its group moves between a root
            // section and a plain group (curated↔discovered shadowing) — recomputed
            // upstream in `reconcile_ops`, so it must be applied here too, not left
            // stale from the prior render.
            t.primary.spec.tree = tree;
            t.primary.spec.tree_path = tree_path;
        }
    }

    /// The display title of tab `id` (for a detached window's banner), or `None` if unknown.
    pub fn tab_title(&self, id: &str) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
    }

    /// Map a surface handle back to (tab id, which pane). Both panes are searched — a
    /// secondary's bell, notification or child-exit is otherwise silently unroutable,
    /// which is exactly how a split tab would stop badging.
    pub fn locate_surface(&self, surface_id: usize) -> Option<(String, PaneIdx)> {
        for t in &self.tabs {
            for (which, pane) in [
                (PaneIdx::Primary, Some(&t.primary)),
                (PaneIdx::Secondary, t.secondary.as_ref()),
            ] {
                if let Some(p) = pane {
                    if let TabSlot::Spawned(s) = &p.slot {
                        if s.id() == surface_id {
                            return Some((t.id.clone(), which));
                        }
                    }
                }
            }
        }
        None
    }

    /// The id of the tab owning the spawned surface with handle `surface_id`
    /// (`GhosttySurface::id`), if any — routes a per-surface signal (bell /
    /// notification) back to its tab. Thin wrapper over `locate_surface` for
    /// callers that don't care which pane.
    pub fn tab_of_surface(&self, surface_id: usize) -> Option<&str> {
        let (id, _) = self.locate_surface(surface_id)?;
        self.tabs.iter().find(|t| t.id == id).map(|t| t.id.as_str())
    }

    /// The currently-active (on-screen) tab id, if any.
    pub fn active_tab(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// The last reported rect for whichever hole pane `which` occupies.
    fn rect_for(&self, which: PaneIdx) -> PixelRect {
        match which {
            PaneIdx::Primary => self.last_rect,
            PaneIdx::Secondary => self.last_rect_secondary,
        }
    }

    /// Ensure the entry at `idx` is spawned (lazy materialization). A cold tab —
    /// never-opened or previously unloaded — spawns a fresh surface from its spec.
    /// A spawn failure leaves the tab cold and returns the error (the caller
    /// surfaces it); the tab can be retried by activating it again. Never panics.
    /// Spawn pane `which` of tab `idx` if it is `Cold`. Returns `true` iff it spawned **this
    /// call** (the pane was cold) — the signal `activate` forwards so a caller can arm a
    /// session-start await only when a fresh surface actually ran its `initial_input` (an
    /// already-live pane starts nothing). `Ok(false)`, never a panic, if the pane doesn't exist
    /// (e.g. `Secondary` on an unsplit tab).
    fn ensure_spawned(&mut self, idx: usize, which: PaneIdx) -> Result<bool, SurfaceError> {
        let Some(pane) = self.tabs[idx].pane(which) else {
            return Ok(false);
        };
        if !matches!(pane.slot, TabSlot::Cold) {
            return Ok(false);
        }
        let spec = pane.spec.clone();
        // Born at ITS OWN hole's rect, not the primary's — see the doc comment on
        // `last_rect_secondary`. A freshly-spawned secondary that started at the primary's
        // rect would render at the wrong width for one frame.
        let rect = self.rect_for(which);
        let s = GhosttySurface::new(self.ns_window, rect, &spec)?;
        self.tabs[idx]
            .pane_mut(which)
            .expect("pane existed above")
            .slot = TabSlot::Spawned(s);
        Ok(true)
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
        // Act on every pane: a cold split respawns split, so `secondary` (if any)
        // stays present and only its slot moves to `Cold` alongside the primary's.
        let mut any_closed = false;
        for pane in self.tabs[idx].panes_mut() {
            match std::mem::replace(&mut pane.slot, TabSlot::Cold) {
                TabSlot::Spawned(s) => {
                    s.close();
                    any_closed = true;
                }
                TabSlot::Cold => {} // nothing live to kill; stays Cold
                TabSlot::Detached => {
                    // No local surface to kill — it lives in another window's
                    // registry (a pop-out). Restore the placeholder; leaving it
                    // `Cold` here would make it look locally respawnable, which
                    // would spawn a second surface for the same tab identity.
                    pane.slot = TabSlot::Detached;
                }
            }
        }
        // A config split the user ✕'d comes back on relaunch — this is the relaunch. Declared
        // cold; the next `activate` spawns it with the primary.
        if self.tabs[idx].secondary.is_none() && self.tabs[idx].primary.spec.split.is_some() {
            self.split(id);
        }
        if !any_closed {
            return None;
        }
        if self.active.as_deref() == Some(id) {
            return self.lean_to_live_neighbour(idx);
        }
        None
    }

    /// The active tab at `idx` just lost its terminal: move `active` to an already-**live**
    /// neighbour and return its id, or `None` (hole left blank) when nothing live remains — never
    /// spawning a cold tab just to fill the hole. Shared by `unload` and `clear_detached`, the two
    /// ways a tab's terminal ends, so both leave the sidebar in the same place.
    fn lean_to_live_neighbour(&mut self, idx: usize) -> Option<String> {
        self.active = None;
        let live: Vec<bool> = self
            .tabs
            .iter()
            .map(|t| t.panes().any(|p| matches!(p.slot, TabSlot::Spawned(_))))
            .collect();
        let n = shell_core::pick_live_neighbour(idx, &live)?;
        let next = self.tabs[n].id.clone();
        // The neighbour is already live (pick_live_neighbour only returns spawned tabs), so
        // this activate never spawns and can't fail.
        let _ = self.activate(&next);
        Some(next)
    }

    /// Retire tab `id`'s `Detached` placeholders to `Cold`: its popped-out surfaces ended (the
    /// primary's child exited inside the detached window — `WindowManager::handle_child_exited`)
    /// and are never coming back through `attach`. A `Cold` secondary never left and is
    /// untouched. Returns what `unload` would: the live neighbour the sidebar leans to when this
    /// was the active tab, else `None` — the same "this tab's terminal is gone" outcome, reached
    /// from another window.
    pub fn clear_detached(&mut self, id: &str) -> Option<String> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        let mut any = false;
        for pane in self.tabs[idx].panes_mut() {
            if matches!(pane.slot, TabSlot::Detached) {
                pane.slot = TabSlot::Cold;
                any = true;
            }
        }
        if !any {
            return None;
        }
        if self.active.as_deref() == Some(id) {
            return self.lean_to_live_neighbour(idx);
        }
        None
    }

    /// Detach tab `id`'s live surfaces for a move to another window's `Registry`
    /// (pop-out): extracts the `Spawned` surface of **every** pane the tab has and
    /// leaves each slot `Detached` — a placeholder that keeps the tab present for
    /// `reconcile` (not missing → no respawn; not stale → no duplicate) while the
    /// actual `GhosttySurface`s move to the caller (the manager reparents their
    /// AppKit views and calls `attach` on the destination registry). The surfaces
    /// are **returned, never closed** — closing one here would kill the PTY the
    /// whole feature exists to preserve.
    ///
    /// Pop-out is whole-tab, so a split tab travels as a pair: `(primary,
    /// Some(secondary))`. The secondary is `None` both for an unsplit tab and for a
    /// split tab whose second pane is `Cold` (nothing live to carry — that slot stays
    /// `Cold` and simply respawns in place on the next `activate`). The pane count the
    /// caller lays the detached window out with therefore derives from what actually
    /// came back, never from `is_split`: a hole with no surface behind it is a
    /// transparent leak to the desktop.
    ///
    /// `None` (nothing extracted, **no slot touched**) for: an unknown id; a `Cold`
    /// primary (restored to `Cold`, not left `Detached`, since there is no surface
    /// anywhere to stand the placeholder in for); or a primary already `Detached`
    /// (restored to `Detached` — already gone elsewhere, calling this twice is a
    /// no-op, not a second extraction). The primary is decided **first**, so a
    /// refused detach leaves the secondary untouched rather than half-extracting a
    /// tab that isn't going anywhere.
    pub fn detach(&mut self, id: &str) -> Option<(GhosttySurface, Option<GhosttySurface>)> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        let primary = match std::mem::replace(&mut self.tabs[idx].primary.slot, TabSlot::Detached) {
            TabSlot::Spawned(s) => s,
            TabSlot::Cold => {
                self.tabs[idx].primary.slot = TabSlot::Cold;
                return None;
            }
            TabSlot::Detached => {
                self.tabs[idx].primary.slot = TabSlot::Detached;
                return None;
            }
        };
        let secondary = match self.tabs[idx].secondary.as_mut() {
            None => None,
            Some(p) => match std::mem::replace(&mut p.slot, TabSlot::Detached) {
                TabSlot::Spawned(s) => Some(s),
                TabSlot::Cold => {
                    p.slot = TabSlot::Cold;
                    None
                }
                TabSlot::Detached => {
                    p.slot = TabSlot::Detached;
                    None
                }
            },
        };
        Some((primary, secondary))
    }

    /// Bring tab `id` live in place if it is `Cold`, spawning its surface(s) against this
    /// window — so a never-opened tab can be popped out (pop-out then `detach`s the now-live
    /// surfaces and reparents them into its own window). No-op for a pane already `Spawned`
    /// or `Detached` (a detached tab's live surface is in another window — never respawn a
    /// second one), and a no-op returning `Ok` if `id` is unknown (the caller's follow-up
    /// `detach` surfaces the not-found).
    ///
    /// A split tab's SECOND pane is spawned too, best-effort: pop-out takes the whole tab, so
    /// leaving a cold secondary behind would pop the tab out at half its content and strand
    /// the other pane in the origin. Its error is discarded rather than shadowing the
    /// primary's — the same asymmetry `activate` uses, and for the same reason: a secondary
    /// that won't spawn must not cost the user the pop-out. `detach` then simply carries one
    /// surface instead of two, and the detached window lays out one hole.
    ///
    /// That second spawn is gated on the PRIMARY being live **in this registry** — the same
    /// gate `activate` carries, and the two must stay in step — which is
    /// what keeps the whole call a no-op on an already-`Detached` tab: its surfaces are in
    /// another window, `detach` will refuse, and spawning its secondary here would leave a
    /// stray surface in a window the tab isn't showing in — with no pop-out to carry it out
    /// again.
    pub fn ensure_spawned_by_id(&mut self, id: &str) -> Result<(), SurfaceError> {
        if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
            self.ensure_spawned(idx, PaneIdx::Primary)?;
            if matches!(self.tabs[idx].primary.slot, TabSlot::Spawned(_))
                && self.tabs[idx].secondary.is_some()
            {
                let _ = self.ensure_spawned(idx, PaneIdx::Secondary);
            }
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
    /// PTY) — the surfaces are handed **back** as `Err((primary, secondary))`
    /// rather than silently dropped. A `bool` return (as the task brief's other
    /// suggested shape) was rejected for this reason: on the failure path the
    /// parameters would simply fall out of scope and hit `GhosttySurface`'s
    /// `Drop` safety net, closing surfaces the caller doesn't expect to lose.
    /// Returning them keeps that decision with the caller instead of making it
    /// silently here.
    ///
    /// **Both panes land, or neither does.** Every slot is validated before any is
    /// written, so a refusal hands the caller back exactly what it passed in and the
    /// registry is unchanged — there is no "the primary landed, the secondary didn't"
    /// state for a caller to discover, and no shape in the `Err` that could express one.
    ///
    /// A returning `secondary` whose tab has **no** secondary pane recreates the pane
    /// (`secondary_spec`, the same derivation `split` uses) rather than refusing. That
    /// is a real path, not a defensive one: a runtime (⌘D) split lives in the chrome and the
    /// registry, not the config, so an origin window the user closed while the tab was popped
    /// out is rebuilt *unsplit* — and refusing there would drop a live PTY on the floor
    /// purely because the slot it left from had since been rebuilt without it.
    pub fn attach(
        &mut self,
        id: &str,
        primary: GhosttySurface,
        secondary: Option<GhosttySurface>,
    ) -> Result<(), (GhosttySurface, Option<GhosttySurface>)> {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return Err((primary, secondary));
        };
        // Validate BOTH slots first — see the all-or-nothing note above.
        if matches!(t.primary.slot, TabSlot::Spawned(_)) {
            return Err((primary, secondary));
        }
        if secondary.is_some()
            && t.secondary
                .as_ref()
                .is_some_and(|p| matches!(p.slot, TabSlot::Spawned(_)))
        {
            return Err((primary, secondary));
        }
        t.primary.slot = TabSlot::Spawned(primary);
        match secondary {
            Some(s) => match t.secondary.as_mut() {
                Some(p) => p.slot = TabSlot::Spawned(s),
                None => {
                    let spec = secondary_spec(&t.primary.spec, id);
                    t.secondary = Some(Pane {
                        spec,
                        slot: TabSlot::Spawned(s),
                    });
                }
            },
            None => {
                // A secondary that left with the tab (`Detached`) and didn't come back ended
                // while it was out — its child exited in the detached window and
                // `handle_child_exited` closed it. Retire the pane, or the tab reads as split
                // forever around a placeholder no `attach` will ever fill. A `Cold` secondary
                // never left and stays: it respawns on the next `activate`. (Needs a live
                // surface to exercise, so it is pinned by inspection only — the same test-double
                // gap `docs/FOLLOWUPS.md` records for `activate`'s gate.)
                if t.secondary
                    .as_ref()
                    .is_some_and(|p| matches!(p.slot, TabSlot::Detached))
                {
                    t.secondary = None;
                    t.focused = PaneIdx::Primary;
                }
            }
        }
        Ok(())
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
        // Spawn every cold pane. The primary's Result is what the caller sees; a secondary
        // spawn failure doesn't block showing the tab, so it's attempted best-effort and its
        // error discarded rather than shadowing the primary's.
        //
        // Gated on the primary being live **in this registry**, exactly as
        // `ensure_spawned_by_id` gates its own second spawn — the two sites must stay in step,
        // because they answer one question ("may this registry birth a secondary surface?") and
        // a registry that answered it two ways is how the stray gets born. Concretely: pop out a
        // split tab whose secondary spawn failed and the primary is `Detached` while the
        // secondary is still `Cold`, so an ungated `activate` on that id would spawn a surface
        // in the ORIGIN window for a tab that isn't showing there — with no pop-out left to
        // carry it out again.
        let spawned = self.ensure_spawned(idx, PaneIdx::Primary);
        if matches!(self.tabs[idx].primary.slot, TabSlot::Spawned(_))
            && self.tabs[idx].secondary.is_some()
        {
            let _ = self.ensure_spawned(idx, PaneIdx::Secondary);
        }
        for (i, t) in self.tabs.iter().enumerate() {
            for (which, pane) in t.panes_indexed() {
                if let TabSlot::Spawned(s) = &pane.slot {
                    if i == idx {
                        // Each pane gets ITS OWN hole's last-reported rect, not a shared
                        // one — see `rect_for`'s doc comment. Applying the primary's rect
                        // to the secondary here would immediately undo the size
                        // `ensure_spawned` just gave a freshly-spawned secondary above.
                        s.set_frame(self.rect_for(which));
                        s.show();
                        // Show every pane of the active tab, but focus only the one
                        // `focused` names — carried finding from Task 2's review: this
                        // used to call `.focus()` on every spawned pane, so whichever
                        // pane was iterated last silently won by construction order.
                        if which == t.focused {
                            s.focus();
                        }
                    } else {
                        s.hide();
                    }
                }
            }
        }
        self.active = Some(id.to_string());
        spawned
    }

    /// Apply a hole's rect to the pane that occupies it, and remember it for later spawns.
    pub fn set_pane_frame(&mut self, which: PaneIdx, rect: PixelRect) {
        match which {
            PaneIdx::Primary => self.last_rect = rect,
            PaneIdx::Secondary => self.last_rect_secondary = rect,
        }
        let Some(active) = self.active.clone() else {
            return;
        };
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == active) {
            if let Some(p) = t.pane_mut(which) {
                if let TabSlot::Spawned(s) = &p.slot {
                    s.set_frame(rect);
                }
            }
        }
    }

    #[cfg(test)]
    pub fn last_rect_for_test(&self, which: PaneIdx) -> PixelRect {
        self.rect_for(which)
    }

    /// Remove a tab; close its surface if spawned.
    pub fn remove(&mut self, id: &str) {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            let mut entry = self.tabs.remove(pos);
            for pane in entry.panes_mut() {
                if let TabSlot::Spawned(s) = std::mem::replace(&mut pane.slot, TabSlot::Cold) {
                    s.close();
                }
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
        for mut entry in self.tabs.drain(..) {
            for pane in entry.panes_mut() {
                if let TabSlot::Spawned(s) = std::mem::replace(&mut pane.slot, TabSlot::Cold) {
                    s.close();
                }
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
            split: None,
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
            split: None,
        }
    }

    #[test]
    fn a_new_tab_is_unsplit() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        assert!(!r.is_split("a"));
    }

    #[test]
    fn is_split_of_unknown_tab_is_false() {
        let r = Registry::new(std::ptr::null_mut(), rect());
        assert!(!r.is_split("nope"));
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
    fn warn_tracks_the_dir_live_in_both_directions() {
        // `warn` is derived per render, never cached at add-time. Caching it made the
        // marker lie whenever the directory's existence changed *after* the tab was
        // materialized — the real case: a config edit declared a tab 34s before the
        // repo was cloned into place, so the ⚠ stuck for the rest of the session.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", dir.to_str().unwrap()), false);
        assert!(r.tab_dtos()[0].warn, "absent at add time → warn");

        std::fs::create_dir(&dir).unwrap();
        assert!(
            !r.tab_dtos()[0].warn,
            "dir appeared after add → warn must clear without re-adding the tab"
        );

        std::fs::remove_dir(&dir).unwrap();
        assert!(
            r.tab_dtos()[0].warn,
            "dir removed after add → warn must return"
        );
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
                split: None,
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
                split: None,
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
    fn detach_of_a_split_cold_tab_leaves_both_panes_alone() {
        // The primary is decided first: a refused detach must not half-extract the tab by
        // marking the SECONDARY detached on the way past. Nothing is live here, so nothing
        // moves — and neither slot may end up a placeholder for a surface that is still here.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        assert!(r.detach("t0").is_none());
        assert!(!r.is_pane_detached("t0", PaneIdx::Primary));
        assert!(
            !r.is_pane_detached("t0", PaneIdx::Secondary),
            "the second pane must not be marked Detached when nothing was detached"
        );
        assert!(r.is_split("t0"), "the tab is still split, just cold");
    }

    #[test]
    fn detach_of_an_already_detached_split_tab_is_a_noop_on_both_panes() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        r.force_detached_pane("t0", PaneIdx::Primary);
        r.force_detached_pane("t0", PaneIdx::Secondary);
        assert!(
            r.detach("t0").is_none(),
            "nothing live here to extract a second time"
        );
        assert!(r.is_pane_detached("t0", PaneIdx::Primary));
        assert!(
            r.is_pane_detached("t0", PaneIdx::Secondary),
            "both placeholders must survive — their surfaces are in the popped-out window"
        );
    }

    #[test]
    fn a_detached_split_tab_reads_as_detached_and_not_secondary_spawned() {
        // What the chrome sees for a popped-out split: one detached row, no local surface in
        // either pane (so neither hole is claimed live while the terminals are elsewhere).
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        r.force_detached_pane("t0", PaneIdx::Primary);
        r.force_detached_pane("t0", PaneIdx::Secondary);
        let dto = r.tab_dtos().into_iter().find(|d| d.id == "t0").unwrap();
        assert!(dto.detached);
        assert!(!dto.spawned);
        assert!(!dto.secondary_spawned);
    }

    #[test]
    fn unload_of_a_detached_split_tab_keeps_both_placeholders() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        r.force_detached_pane("t0", PaneIdx::Primary);
        r.force_detached_pane("t0", PaneIdx::Secondary);
        assert_eq!(r.unload("t0"), None, "no local surfaces to kill");
        assert!(r.is_pane_detached("t0", PaneIdx::Primary));
        assert!(
            r.is_pane_detached("t0", PaneIdx::Secondary),
            "a Detached secondary must not fall back to Cold — that would make it look \
             locally respawnable and duplicate the popped-out surface"
        );
    }

    #[test]
    fn attach_recreates_a_missing_secondary_slot_with_the_split_spec() {
        // `attach` recreating the slot (redock into an origin that was closed and rebuilt
        // unsplit) must produce the SAME spec `split` does, or the returning surface's
        // events route nowhere. Both go through `secondary_spec`; this pins that they agree.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        let from_split = r.secondary_spec_for_test("t0").unwrap();
        let from_attach = secondary_spec(&spec("t0", "/tmp"), "t0");
        assert_eq!(from_split.id, from_attach.id, "same surface id");
        assert_eq!(from_split.dir, from_attach.dir);
        assert_eq!(from_split.startup, from_attach.startup);
        assert_eq!(from_attach.id, "t0::2");
        assert!(
            from_attach.startup.is_none(),
            "the second pane runs a shell"
        );
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
    // surfaces failure arm (a `Spawned` slot) need a live surface to even call it
    // with. The slot gate it relies on is exercised structurally above via
    // `force_detached` + `is_detached`, and the one piece of its two-pane path that
    // needs no surface — the spec it recreates a missing secondary slot with — is
    // pinned by `attach_recreates_a_missing_secondary_slot_with_the_split_spec`.
    // See the task report for the GUI-driven verification plan.

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
        // Same for its SECOND pane: this call spawns a cold secondary for the pop-out that
        // follows, but a Detached tab has no pop-out to follow — a surface spawned here would
        // sit in a window the tab isn't in, with nothing to carry it out again.
        r.split("t0");
        assert!(r.ensure_spawned_by_id("t0").is_ok());
        assert!(
            !r.is_spawned("t0"),
            "neither pane of a Detached tab may spawn locally"
        );
        assert!(
            r.ensure_spawned_by_id("does-not-exist").is_ok(),
            "unknown id is a clean no-op"
        );
    }

    #[test]
    fn activate_never_spawns_a_secondary_for_a_detached_tab() {
        // The same gate `ensure_spawned_by_id` carries, asserted on the OTHER site that can birth
        // a secondary — the two must stay in step. Reachable shape: pop out a split tab whose
        // secondary spawn failed, so the primary is Detached and the secondary still Cold; an
        // ungated `activate` on that id would put a surface in the ORIGIN window for a tab that
        // isn't showing there, with no pop-out left to carry it out again.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        r.force_detached_pane("t0", PaneIdx::Primary);
        assert!(r.activate("t0").is_ok());
        assert!(
            !r.is_spawned("t0"),
            "neither pane of a Detached tab may spawn locally"
        );
    }

    #[test]
    fn tab_dto_carries_the_structural_split_flag_not_just_liveness() {
        // `split` is STRUCTURE (does a secondary pane exist), `secondary_spawned` is LIVENESS.
        // Forwarding only the second is what let a hot-reload rebuild (remove + add → unsplit)
        // leave the chrome rendering a divider over a tab with no second pane.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        let dto = |r: &Registry| r.tab_dtos().into_iter().find(|d| d.id == "a").unwrap();
        assert!(!dto(&r).split, "a fresh tab is unsplit");
        r.split("a");
        let d = dto(&r);
        assert!(d.split, "structurally split the instant `split` runs…");
        assert!(!d.secondary_spawned, "…even though nothing is live yet");
        r.close_secondary("a");
        assert!(!dto(&r).split);
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

    // --- split / close_secondary / focus_pane (Task 3) -----------------------

    #[test]
    fn split_adds_a_secondary_and_close_removes_it() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        r.split("a");
        assert!(r.is_split("a"));
        assert!(r.close_secondary("a"));
        assert!(!r.is_split("a"));
    }

    fn spec_with_split(id: &str, cmd: Option<&str>) -> TabSpec {
        let mut s = spec(id, "/tmp/a");
        s.split = Some(warden_config::Split {
            side: warden_config::SplitSide::Left,
            size: 0.3,
            startup: cmd.map(String::from),
        });
        s
    }

    #[test]
    fn add_with_config_split_declares_the_secondary_cold_with_its_cmd() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec_with_split("a", Some("scratch")), false)
            .unwrap();
        assert!(
            r.is_split("a"),
            "a config split is declared at add, not on first ⌘D"
        );
        let sec = r.secondary_spec_for_test("a").unwrap();
        assert_eq!(sec.id, "a::2");
        assert_eq!(sec.startup.as_deref(), Some("scratch"));
        assert_eq!(
            sec.split, None,
            "the second pane carries no split of its own"
        );
        let dto = &r.tab_dtos()[0];
        assert!(dto.split && !dto.secondary_spawned);
        let layout = dto.split_layout.as_ref().unwrap();
        assert_eq!((layout.side, layout.size), ("left", 0.3));
    }

    #[test]
    fn config_split_closed_by_x_comes_back_on_unload() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec_with_split("a", None), false).unwrap();
        assert!(r.close_secondary("a"));
        assert!(!r.is_split("a"));
        // Nothing is live, so `unload` returns None — but it still re-declares the pane, which
        // is what "✕ holds only until the tab is relaunched" means structurally.
        assert_eq!(r.unload("a"), None);
        assert!(r.is_split("a"));
        assert_eq!(r.tab_dtos()[0].split_layout.as_ref().unwrap().size, 0.3);
    }

    #[test]
    fn runtime_split_on_unsplit_config_has_no_layout_and_no_cmd() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        r.split("a");
        assert_eq!(r.secondary_spec_for_test("a").unwrap().startup, None);
        assert!(r.tab_dtos()[0].split_layout.is_none());
    }

    #[test]
    fn set_meta_updates_the_split_layout_live() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec_with_split("a", None), false).unwrap();
        r.set_meta(
            "a",
            &warden_config::TabMeta {
                group: None,
                probe: None,
                kill: None,
                title: "a".into(),
                split: Some(warden_config::Split {
                    side: warden_config::SplitSide::Right,
                    size: 0.6,
                    startup: None,
                }),
            },
            false,
            Vec::new(),
        );
        let layout = r.tab_dtos()[0].split_layout.clone().unwrap();
        assert_eq!((layout.side, layout.size), ("right", 0.6));
    }

    #[test]
    fn split_twice_is_a_noop_not_a_third_pane() {
        // "Not a third pane" is enforced by the TYPE, not this assertion — `TabEntry.secondary`
        // is `Option<Pane>`, not a `Vec`, so a third pane is unrepresentable regardless of how
        // many times `split` runs. What this proves is the idempotency half: a second `split`
        // call doesn't somehow clear `secondary` back to `None`.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        r.split("a");
        r.split("a");
        assert!(r.is_split("a"));
    }

    #[test]
    fn close_secondary_on_unsplit_tab_is_false() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        assert!(!r.close_secondary("a"));
    }

    #[test]
    fn the_secondary_runs_a_bare_shell_in_the_tabs_dir() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s = spec("a", "/tmp/a");
        s.startup = Some("amux".into());
        r.add(&s, false).unwrap();
        r.split("a");
        let sec = r.secondary_spec_for_test("a").expect("secondary present");
        assert_eq!(sec.dir, std::path::PathBuf::from("/tmp/a"));
        assert_eq!(sec.shell, s.shell);
        assert_eq!(
            sec.startup, None,
            "the scratch pane must not inherit the tab's cmd"
        );
        assert_ne!(sec.id, s.id, "panes need distinct ids for surface routing");
    }

    #[test]
    fn closing_the_secondary_returns_focus_to_the_primary() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        r.split("a");
        assert!(r.focus_pane("a", PaneIdx::Secondary));
        assert_eq!(r.focused_pane("a"), Some(PaneIdx::Secondary));
        r.close_secondary("a");
        assert_eq!(r.focused_pane("a"), Some(PaneIdx::Primary));
    }

    #[test]
    fn a_fresh_split_pane_is_cold_not_live() {
        // `is_spawned` is any-pane (see its own doc comment), but that's sufficient here: with
        // `ns_window` null and no `activate` call, neither pane can leave `Cold`, so "not spawned
        // at all" already proves both the fresh secondary and the lazily-added primary are cold.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        r.split("a");
        assert!(r.is_split("a"));
        assert!(!r.is_spawned("a"), "split must not spawn either pane");
    }

    #[test]
    fn focusing_a_missing_secondary_is_refused() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        assert!(!r.focus_pane("a", PaneIdx::Secondary));
        assert_eq!(r.focused_pane("a"), Some(PaneIdx::Primary));
    }

    // --- activate's show/focus loop must honour `focused`, not "last spawned wins" ---
    //
    // Carried finding from Task 2's review: `activate` used to call `.focus()` on every
    // spawned pane of the active tab, ignoring `focused` entirely — inert only because no
    // secondary existed yet. It's fixed by iterating `TabEntry::panes_indexed()` and
    // comparing each pane's `PaneIdx` against `t.focused`.
    //
    // That fix cannot be exercised through `Registry::activate` itself in this file's unit
    // tests: every test here runs against `ns_window = null` so panes never leave `Cold`
    // (see the detach/attach block earlier in this file), and calling `activate` was verified to SIGSEGV
    // even on a cold, unsplit tab — `GhosttySurface::new`'s null-window path is not the
    // "safely returns Err" case it looks like; the crash was reproduced and is why no test
    // in this file calls `activate` directly. So this test pins the one ingredient that
    // *is* safely testable: that `panes_indexed()` — what the loop's `which == t.focused`
    // comparison is applied to — pairs each pane with its own correct `PaneIdx`, primary
    // first. A wrong pairing here (e.g. the two panes swapped) is exactly the kind of bug
    // that would silently misroute focus; the `.focus()`/`.show()` call sequencing itself
    // is GUI-only verifiable, the same boundary already documented for `attach`/`detach`.
    #[test]
    fn panes_indexed_pairs_each_pane_with_its_own_idx_primary_first() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        let t = r.tabs.iter().find(|t| t.id == "a").unwrap();
        let idxs: Vec<PaneIdx> = t.panes_indexed().map(|(w, _)| w).collect();
        assert_eq!(idxs, vec![PaneIdx::Primary], "unsplit tab: primary only");

        r.split("a");
        let t = r.tabs.iter().find(|t| t.id == "a").unwrap();
        let idxs: Vec<PaneIdx> = t.panes_indexed().map(|(w, _)| w).collect();
        assert_eq!(
            idxs,
            vec![PaneIdx::Primary, PaneIdx::Secondary],
            "split tab: primary then secondary, in that order"
        );
        for (which, pane) in t.panes_indexed() {
            assert_eq!(
                pane.spec.id,
                t.pane(which).unwrap().spec.id,
                "panes_indexed's PaneIdx must resolve back to the SAME pane via TabEntry::pane"
            );
        }
    }

    // --- per-pane geometry (Task 4) -------------------------------------------

    #[test]
    fn a_pane_rect_is_remembered_for_later_spawns() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        let wide = PixelRect {
            x: 1.0,
            y: 2.0,
            width: 300.0,
            height: 400.0,
        };
        r.set_pane_frame(PaneIdx::Primary, wide);
        assert_eq!(r.last_rect_for_test(PaneIdx::Primary), wide);
    }

    #[test]
    fn pane_rects_are_remembered_independently() {
        // Task 4 introduced `last_rect`/`last_rect_secondary` as two separate fields, but its
        // own test only ever set the primary's — nothing pinned that the two don't alias.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        let wide = PixelRect {
            x: 1.0,
            y: 2.0,
            width: 300.0,
            height: 400.0,
        };
        let narrow = PixelRect {
            x: 5.0,
            y: 6.0,
            width: 30.0,
            height: 40.0,
        };
        r.set_pane_frame(PaneIdx::Primary, wide);
        r.set_pane_frame(PaneIdx::Secondary, narrow);
        assert_eq!(r.last_rect_for_test(PaneIdx::Primary), wide);
        assert_eq!(r.last_rect_for_test(PaneIdx::Secondary), narrow);
    }

    // --- surface→tab routing (Task 5) ------------------------------------------

    #[test]
    fn pane_index_round_trips_and_floors_to_primary() {
        assert_eq!(PaneIdx::Primary.index(), 0);
        assert_eq!(PaneIdx::Secondary.index(), 1);
        assert_eq!(PaneIdx::from_index(0), PaneIdx::Primary);
        assert_eq!(PaneIdx::from_index(1), PaneIdx::Secondary);
        assert_eq!(PaneIdx::from_index(7), PaneIdx::Primary, "never an error");
    }

    #[test]
    fn clear_detached_returns_every_detached_pane_to_cold() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        r.force_detached_pane("t0", PaneIdx::Primary);
        r.force_detached_pane("t0", PaneIdx::Secondary);
        assert!(r.is_detached("t0"));
        // Not the active tab, nothing live elsewhere → no neighbour to lean to.
        assert_eq!(r.clear_detached("t0"), None);
        assert!(!r.is_detached("t0"));
        assert!(!r.is_pane_detached("t0", PaneIdx::Secondary));
        assert!(
            r.is_split("t0"),
            "the split structure survives; only the slots go cold"
        );
        assert!(!r.is_spawned("t0"));
    }

    #[test]
    fn clear_detached_leaves_a_cold_secondary_and_unknown_tabs_alone() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let _ = r.add(&spec("t0", "/tmp"), false);
        r.split("t0");
        r.force_detached_pane("t0", PaneIdx::Primary); // secondary stayed Cold (never carried)
        assert_eq!(r.clear_detached("t0"), None);
        assert!(!r.is_detached("t0"));
        assert!(r.is_split("t0"));
        assert_eq!(
            r.clear_detached("t0"),
            None,
            "nothing detached any more → no-op"
        );
        assert_eq!(r.clear_detached("nope"), None);
    }

    #[test]
    fn locate_surface_is_none_for_an_unknown_id() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        assert_eq!(r.locate_surface(999_999), None);
    }
}
