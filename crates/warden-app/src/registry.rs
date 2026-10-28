use crate::rex::size::PaneSize;
use crate::rex::PaneTarget;
use crate::surface::{ghostty::GhosttySurface, PixelRect, SurfaceError, TabSpec, TerminalSurface};
use std::os::raw::c_void;
use std::path::PathBuf;

/// One probe-enabled tab's work item: `(id, dir, title, probe_cmd)`. The probe
/// runner substitutes `{dir}`/`{title}` into `probe_cmd` and runs it with cwd = dir.
pub type ProbeTarget = (String, std::path::PathBuf, String, String);

/// How a tab's session ends: a configured command to run (`dir`, `title`, `cmd` — the caller
/// substitutes `{dir}`/`{title}` and runs it with cwd = dir), or the Rex sessions of these pane
/// keys to destroy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndTarget {
    Command(PathBuf, String, String),
    Rex(Vec<String>),
}

/// A Rex pane's session to find or create — `RexHub::ensure_session`'s arguments — and the token
/// its answer carries back to [`Registry::rex_opened`]. A main pane holding agents (`agents`) is
/// opened by `RexHub::open_agents` instead, which reads `auto` and `resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RexOpen {
    pub pane_key: String,
    pub server: String,
    pub dir: PathBuf,
    pub startup: Option<String>,
    pub label: String,
    pub token: u64,
    /// Opened by warden itself (launch, `load_on_open`, `remember_tabs`, hot reload) rather than
    /// by the user selecting the tab: attaches what is live and wakes nothing.
    pub auto: bool,
    /// The pane is a Rex tab's main pane, whose session's windows are its agent tabs.
    pub agents: bool,
    /// The tab's `resume` program as configured now.
    pub resume: Option<String>,
}

/// What an agent pane's open found: each live agent tab's terminal, in list order, and the one
/// to show. Empty when nothing is live (every tab asleep, or none yet on an automatic open).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsOpened {
    pub agents: Vec<(String, PaneTarget)>,
    pub selected: Option<String>,
}

/// A Rex open's answer: the one terminal a pane shows, or an agent pane's agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RexAnswer {
    Pane(PaneTarget),
    Agents(AgentsOpened),
}

/// The Rex pane key and surface id of agent `record_id` in tab `tab_id`'s main pane. The one
/// spelling: the registry, the hub's per-agent bindings and size claims all use it.
pub(crate) fn agent_key(tab_id: &str, record_id: &str) -> String {
    format!("{tab_id}::agent:{record_id}")
}

/// Starts a [`RexOpen`] somewhere that may block, and answers later through
/// [`Registry::rex_opened`] on the main thread. Must not block its caller, nor answer from
/// inside the call. `Err` (the reason) when it couldn't start, so nothing will answer.
pub type RexOpener = Box<dyn Fn(RexOpen) -> Result<(), String> + Send>;

/// What became of a Rex open's answer.
#[derive(Debug)]
pub enum RexLanded {
    /// The pane stopped waiting for it (unloaded, closed, removed, or opened again since).
    Stale,
    /// The session couldn't be opened; the tab's `warn` says why.
    Failed,
    /// The pane went live, or stayed cold because its surface failed to spawn.
    Opened(Result<(), SurfaceError>),
}

/// What became of a tab when one of its agents' relays exited ([`Registry::close_agent`]).
#[derive(Debug, PartialEq, Eq)]
pub enum AgentClosed {
    /// Another agent of the pane is live; the pane shows it if the closed one was on screen.
    PaneLive,
    /// Nothing live was left: the tab went cold, and the sidebar leans to this tab (as `unload`).
    PaneCold(Option<String>),
}

/// What became of a tab whose popped-out surfaces ended ([`Registry::clear_detached`]).
#[derive(Debug, PartialEq, Eq)]
pub enum DetachedEnded {
    /// The tab went cold; the sidebar leans to this tab (as `unload`).
    Cold(Option<String>),
    /// Its main pane still holds a running agent, now selected: the tab stays.
    SiblingShown,
}

/// Where a Rex pane stands on its way to a session. Lives beside the slot, which stays `Cold`
/// until a surface exists.
#[derive(Debug, Clone, PartialEq)]
enum RexState {
    Idle,
    /// An open is in flight; only the answer carrying `token` lands. `wake`: the user opened the
    /// pane meanwhile, so an answer that finds nothing live is followed by a user open.
    Opening {
        token: u64,
        wake: bool,
    },
    /// The session answered; the next spawn runs the relay on it.
    Ready(PaneTarget),
    /// An agent pane's open found nothing live (every agent asleep, or none yet). Only a user open
    /// asks again, since only a user open wakes an agent.
    Asleep,
}

impl RexState {
    /// The token of the open in flight, if one is.
    fn opening(&self) -> Option<u64> {
        match self {
            RexState::Opening { token, .. } => Some(*token),
            _ => None,
        }
    }
}

/// The chrome-facing shape of a declared split — the two layout facts the chrome seeds the
/// divider from. `side` is the wire string the chrome's `.secondary-left` class keys on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SplitLayoutDto {
    pub side: &'static str,
    pub size: f64,
}

/// How the confirm row ends a tab's session: ⏻ `suspend` (restorable) or ☠ `destroy` (the tab's
/// `kill`). Deserialized from the chrome's `end_session` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndSession {
    Suspend,
    Destroy,
}

/// Display descriptor sent to the web chrome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TabDto {
    pub id: String,
    pub title: String,
    /// Why the tab can't open as configured, or `None`: its dir is missing NOW (derived per render,
    /// see `tab_dtos`), or its Rex session couldn't be opened.
    pub warn: Option<String>,
    pub spawned: bool, // surface is live (load_on_open or already focused) vs cold/declared
    pub group: Option<String>, // [[window.group]] membership; None = loose (headerless)
    pub has_probe: bool, // a session-probe command is configured for this tab
    pub has_kill: bool, // a session-kill (destroy) command is configured for this tab
    pub has_suspend: bool, // a session-suspend command is configured for this tab
    pub has_cmd: bool, // a startup command is configured (→ presence dot can offer restart)
    pub tree: bool,    // row belongs to a project-tree (root) section
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
    /// above, which is that pane's LIVENESS. Both are needed: a hot-reload `respawn_tabs`
    /// rebuilds a split tab via `remove` + `add`, i.e. **unsplit** for a runtime (⌘D) split
    /// (only a config split survives, since `add` re-declares it from `TabSpec.split`), and
    /// liveness alone can't tell the chrome its `splitById` is now `false` — it would render a
    /// divider and an empty second pane until the next `onSelect`, which never comes if the tab
    /// stays selected. Carried on every snapshot, so `activate_tab`'s `res.split` is a fast path
    /// rather than the sole reconciler.
    pub split: bool,
    /// The declared (config) split's layout, or `None` for a tab with no config split — a
    /// runtime ⌘D split reports `None` here and `split: true` above. The chrome keys its
    /// "config split vs runtime split" behaviour (where the ratio is remembered, whether a
    /// closed pane stays closed) on this field's presence.
    pub split_layout: Option<SplitLayoutDto>,
    /// The main pane holds agent tabs, so the chrome shows the agent tab list beside it.
    pub agents: bool,
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

    /// The **hole** (left-to-right slice of a detached window's content) this pane occupies —
    /// the one home for the hole↔pane mapping, the detached counterpart of
    /// [`index`](Self::index). shell-core's detach page knows holes only by position
    /// (`DetachSpec.panes` order = report/focus index), and it must not learn what a "split"
    /// is; so a `side = "left"` split — which the docked chrome draws with the secondary
    /// FIRST (`.secondary-left`, a flex `order` over an untouched pane→index contract) — is
    /// mirrored here, on warden's side of the seam: `mirrored` → the secondary is hole 0
    /// and the primary hole 1. Unmirrored (secondary on the right, or a single hole) the
    /// hole is the pane's ordinary `index`. Without this the popped-out tab lays out
    /// primary-left whatever side the config declared.
    pub fn hole(self, mirrored: bool) -> usize {
        match (self, mirrored) {
            (PaneIdx::Primary, true) => 1,
            (PaneIdx::Secondary, true) => 0,
            (which, false) => which.index(),
        }
    }
    /// Inverse of [`hole`](Self::hole): which pane a detached window's hole `i` reports for.
    /// Same floor as [`from_index`](Self::from_index) — anything past the second hole is the
    /// primary, never an error — so a stale rect for a hole that has since collapsed can't
    /// leave a surface unpositioned.
    pub fn from_hole(i: usize, mirrored: bool) -> Self {
        match (i, mirrored) {
            (0, true) => PaneIdx::Secondary,
            (1, true) => PaneIdx::Primary,
            (i, false) => PaneIdx::from_index(i),
            (_, true) => PaneIdx::Primary,
        }
    }
}

/// One agent tab of a Rex tab's main pane: its record, its own surface (a relay on the agent's
/// window's first terminal block, under [`agent_key`]), and the terminal it attaches. The target
/// is kept after a spawn, so a slot whose relay exits reattaches the same block when shown again.
struct AgentSlot {
    record_id: String,
    slot: TabSlot,
    rex: RexState,
}

/// One terminal within a tab: its spawn recipe plus its live/cold/detached slot, and — for a pane
/// whose spec names a `server` — how far its Rex session has got.
///
/// A Rex tab's main pane ([`hosts_agents`]) shows its session's windows as agent tabs: `rex` is
/// the open of the whole pane, `agents` holds one slot per live agent the open found, and only
/// the `selected` one is ever on screen. Its own `slot` stays `Cold` — `Detached` while popped
/// out, and `Spawned` only for a surface that returned from a pop-out with no agent slot left to
/// land in.
struct Pane {
    spec: TabSpec,
    slot: TabSlot,
    rex: RexState,
    agents: Vec<AgentSlot>,
    selected: Option<String>,
}

impl Pane {
    fn new(spec: TabSpec, slot: TabSlot) -> Pane {
        Pane {
            spec,
            slot,
            rex: RexState::Idle,
            agents: Vec::new(),
            selected: None,
        }
    }

    fn agent(&self, record_id: &str) -> Option<&AgentSlot> {
        self.agents.iter().find(|a| a.record_id == record_id)
    }

    fn agent_mut(&mut self, record_id: &str) -> Option<&mut AgentSlot> {
        self.agents.iter_mut().find(|a| a.record_id == record_id)
    }

    /// The slot this pane puts on screen: the selected agent's while it holds agents, else its own.
    fn shown_slot(&self) -> Option<&TabSlot> {
        if self.agents.is_empty() {
            return Some(&self.slot);
        }
        Some(&self.agent(self.selected.as_deref()?)?.slot)
    }

    /// The live surface this pane puts on screen, if any.
    fn shown(&self) -> Option<&GhosttySurface> {
        match self.shown_slot()? {
            TabSlot::Spawned(s) => Some(s),
            _ => None,
        }
    }

    /// Every live surface of this pane, its own and its agents'.
    fn surfaces(&self) -> impl Iterator<Item = &GhosttySurface> {
        std::iter::once(&self.slot)
            .chain(self.agents.iter().map(|a| &a.slot))
            .filter_map(|s| match s {
                TabSlot::Spawned(s) => Some(s),
                _ => None,
            })
    }

    /// Whether any surface of this pane is live.
    fn live(&self) -> bool {
        self.surfaces().next().is_some()
    }

    /// Close every live surface of this pane, leaving `Detached` placeholders in place. Returns
    /// whether one was closed.
    fn close_live(&mut self) -> bool {
        let mut closed = false;
        let agents = self.agents.iter_mut().map(|a| &mut a.slot);
        for slot in std::iter::once(&mut self.slot).chain(agents) {
            if matches!(slot, TabSlot::Spawned(_)) {
                if let TabSlot::Spawned(s) = std::mem::replace(slot, TabSlot::Cold) {
                    s.close();
                    closed = true;
                }
            }
        }
        closed
    }
}

/// The spec a tab's SECOND pane runs: the tab's own shell, in the tab's own dir, with the
/// config split's `cmd` as its startup (a runtime split has none — the frame's scratch
/// terminal), under the distinct surface id `"<tab>::2"` that surface→tab routing
/// (`locate_surface`) and the surface event sink key on — and, for a Rex tab, its Rex pane key,
/// so a runtime (⌘D) split reattaches the same session a config split would. The tab's `server`
/// carries over: a whole project lives on one server.
///
/// Every site that names the second pane goes through here — `split` (the ⌘D path), `attach`
/// and `mark_detached` (which recreate the slot for a popped-out split whose origin was rebuilt
/// from config), `end_target` and `rex_pane_key`. None may re-derive it: two spellings of one spec would drift
/// the second pane's surface id, and a mismatched id routes its bell, notification and
/// child-exit to nothing.
fn secondary_spec(primary: &TabSpec, tab_id: &str) -> TabSpec {
    let mut spec = primary.clone();
    spec.id = secondary_id(tab_id);
    // A config split names what the second pane runs; a runtime split is a bare scratch shell.
    spec.startup = primary.split.as_ref().and_then(|s| s.startup.clone());
    spec.split = None;
    spec
}

/// Tab `tab_id`'s second pane's id (see [`secondary_spec`]), for a reader holding only the tab's
/// key, such as the unclaimed-session list counting a tab's Rex panes.
pub(crate) fn secondary_id(tab_id: &str) -> String {
    format!("{tab_id}::{SECONDARY_SUFFIX}")
}

const SECONDARY_SUFFIX: &str = "2";

/// The tab whose second pane `id` is ([`secondary_id`]), or `None` for any other id.
pub(crate) fn tab_of_secondary(id: &str) -> Option<&str> {
    id.strip_suffix(SECONDARY_SUFFIX)?.strip_suffix("::")
}

/// The Rex pane key of pane `which` of the tab whose primary runs `primary` — that pane's spec
/// id — or `None` for a plain tab. Also reads a popped-out tab, whose origin registry may be gone.
pub(crate) fn rex_pane_key(primary: &TabSpec, which: PaneIdx) -> Option<String> {
    primary.server.as_ref()?;
    Some(match which {
        PaneIdx::Primary => primary.id.clone(),
        PaneIdx::Secondary => secondary_spec(primary, &primary.id).id,
    })
}

/// The Rex key of the surface pane `which` of that tab shows when it is agent `agent`'s (a
/// popped-out agent's: its [`agent_key`]), else [`rex_pane_key`]. What a popped-out surface's
/// size claims and exit check name.
pub(crate) fn shown_rex_key(
    primary: &TabSpec,
    which: PaneIdx,
    agent: Option<&str>,
) -> Option<String> {
    match agent.filter(|_| which == PaneIdx::Primary && primary.server.is_some()) {
        Some(record_id) => Some(agent_key(&primary.id, record_id)),
        None => rex_pane_key(primary, which),
    }
}

/// Whether pane `which` of the tab whose primary runs `primary` holds agent tabs: the main pane of
/// a Rex tab. An unclaimed session's row shows the one session it lists, and a split pane is one
/// terminal.
fn hosts_agents(primary: &TabSpec, which: PaneIdx) -> bool {
    which == PaneIdx::Primary
        && primary.server.is_some()
        && !crate::rex::unclaimed::is_unclaimed(
            &primary.id,
            &primary.dir,
            primary.server.as_deref(),
        )
}

/// The label of the Rex session pane `which` of tab `title` creates. Rex refuses a longer label,
/// so a long title is cut to fit, keeping the pane's suffix.
fn session_label(title: &str, which: PaneIdx) -> String {
    let suffix = match which {
        PaneIdx::Primary => " · main",
        PaneIdx::Secondary => " · split",
    };
    let room = crate::rex::SESSION_LABEL_MAX.saturating_sub(suffix.chars().count());
    title.chars().take(room).chain(suffix.chars()).collect()
}

/// Whether `spec`'s presence comes from its `probe` command: it has one, and isn't a Rex tab.
fn runs_probe(spec: &TabSpec) -> bool {
    spec.probe.is_some() && spec.server.is_none()
}

/// The `suspend` command a tab offers ⏻ with. It runs here, with cwd = the tab's `dir`, so a
/// remote Rex tab — whose `dir` is a path on its server — offers none.
fn suspend_cmd(spec: &TabSpec) -> Option<&String> {
    spec.suspend.as_ref().filter(|_| !is_remote(spec))
}

/// The command ▶ start types into a tab's live shell. A Rex tab has none: its pane runs the
/// relay, so typed text lands in the session's running program, and opening the tab is its start.
fn start_cmd(spec: &TabSpec) -> Option<&String> {
    spec.startup.as_ref().filter(|_| spec.server.is_none())
}

/// Whether `spec` runs on a Rex server other than this Mac's, so its `dir` is a path there.
fn is_remote(spec: &TabSpec) -> bool {
    spec.server
        .as_deref()
        .is_some_and(|s| s != crate::rex::server::LOCAL)
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
    /// Why the last Rex open for one of this tab's panes failed; cleared when one succeeds.
    rex_error: Option<String>,
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
    /// Opens Rex panes' sessions off this thread; `None` until the window installs one, and then
    /// a Rex pane can't open (its tab says so).
    rex_opener: Option<RexOpener>,
    /// The socket Rex panes' relays report to (`rex::reports`); none, they report nowhere.
    report_socket: Option<PathBuf>,
    /// The token the next Rex open carries.
    next_rex_token: u64,
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
            rex_opener: None,
            report_socket: None,
            next_rex_token: 0,
        }
    }

    /// Install what opens this window's Rex panes ([`RexOpener`]).
    pub fn set_rex_opener(&mut self, opener: RexOpener) {
        self.rex_opener = Some(opener);
    }

    /// Install the socket this window's Rex panes' relays report to ([`crate::rex::reports`]).
    pub fn set_report_socket(&mut self, path: Option<PathBuf>) {
        self.report_socket = path;
    }

    /// Add a tab. `load_on_open=true` spawns now (eager); `false` declares it
    /// (lazy — spawns on first `activate`). [spec §3]
    ///
    /// A failed *eager* spawn is non-fatal: the tab is still added as a **cold**
    /// entry (it shows in the sidebar and retries on next `activate`/focus) and
    /// the `SurfaceError` is returned for the caller to surface — never a panic,
    /// since one bad surface must not take down the window. A declared tab can't
    /// fail here (no spawn attempted) → always `Ok`. An eager Rex tab asks for its session here
    /// and spawns when the answer lands (`rex_opened`).
    pub fn add(&mut self, spec: &TabSpec, load_on_open: bool) -> Result<(), SurfaceError> {
        self.tabs.push(TabEntry {
            id: spec.id.clone(),
            title: spec.title.clone(),
            primary: Pane::new(spec.clone(), TabSlot::Cold),
            secondary: None,
            focused: PaneIdx::Primary,
            rex_error: None,
        });
        let mut err = None;
        if load_on_open {
            let idx = self.tabs.len() - 1;
            match self.ensure_spawned(idx, PaneIdx::Primary, true) {
                Ok(_) => self.tabs[idx].primary.surfaces().for_each(|s| s.hide()),
                Err(e) => err = Some(e),
            }
        }
        // A config split is STRUCTURAL: declare the pane now so the tab reads as split from its
        // first snapshot, and spawn it alongside an eager primary so `load_on_open` brings both
        // panes up together (a lazy tab's `activate` already spawns every cold pane). This eager
        // path (`load_on_open` + a config split) is pinned by inspection only — the registry
        // tests run against a null NSWindow and never spawn — the same test-double gap `attach`'s
        // `Detached`-retire branch records.
        if spec.split.is_some() {
            let idx = self.tabs.len() - 1;
            self.split(&spec.id);
            if self.tabs[idx].primary.live() {
                match self.ensure_spawned(idx, PaneIdx::Secondary, true) {
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
            .is_some_and(|t| t.panes().any(Pane::live))
    }

    /// Give tab `id` a second pane: the tab's own shell, in the tab's own dir, running the
    /// config split's `cmd` when the tab has one, else no startup command — the frame's
    /// scratch terminal (`secondary_spec` decides). Idempotent: splitting an already
    /// split tab is a no-op, which is what keeps the two-pane cap true even if the chrome
    /// double-fires. Unknown tab is a no-op.
    ///
    /// The pane is created **Cold**, not spawned: `activate` brings cold panes live, exactly
    /// as a lazily-added tab (`add(load_on_open = false)`) already works. Spawning here would
    /// make this method the only one in the file that must succeed against a live NSWindow,
    /// and would make it untestable — the registry tests run against a null window.
    /// The `split_pane` command calls `activate` right after, so the pane still comes
    /// up immediately for the user.
    ///
    /// The secondary's `TabSpec.id` is `"<tab>::2"`, distinct from the tab id — see
    /// `secondary_spec`, the one home for that derivation.
    ///
    /// Returns whether the tab is split now. An unclaimed Rex session's tab never is: its pane
    /// attaches the session it lists, and a second pane would have to make a session of its own.
    pub fn split(&mut self, id: &str) -> bool {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if t.secondary.is_some() {
            return true;
        }
        let p = &t.primary.spec;
        if crate::rex::unclaimed::is_unclaimed(&p.id, &p.dir, p.server.as_deref()) {
            return false;
        }
        let spec = secondary_spec(&t.primary.spec, id);
        t.secondary = Some(Pane::new(spec, TabSlot::Cold));
        true
    }

    /// Drop tab `id`'s second pane, freeing its surface. Returns whether there was one.
    /// Focus falls back to the primary — the record, and, on the visible tab, the live
    /// surface's first responder too: the closed view held it, and with nothing taking it
    /// over the keystrokes go to the bare NSWindow, which beeps.
    pub fn close_secondary(&mut self, id: &str) -> bool {
        let is_active = self.active.as_deref() == Some(id);
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        // `close()`, never a bare `take()` that lets `Drop` free it: `GhosttySurface::drop`
        // treats being dropped without `close()` as a registry bug and says so in debug
        // builds, so a plain take made the second shell exiting — the way a split ends — print a
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
            if is_active {
                if let Some(s) = t.primary.shown() {
                    s.focus();
                }
            }
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
    /// CAP for nothing. A Rex tab's dot comes from Rex, not the scheduler, so it never has one.
    /// Unknown id ⇒ `false`.
    pub fn tab_has_probe(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && runs_probe(&t.primary.spec))
    }

    /// Stand tab `id` up as popped out without extracting anything: its live surfaces are
    /// already in a detached window. `WindowManager::build_window` calls this for every tab
    /// still out when its origin window is rebuilt (closed, then reopened — by the user or by
    /// `redock` itself), so the rebuilt row is the same `Detached` placeholder `detach` leaves:
    /// `activate`/`ensure_spawned_by_id` spawn nothing for it, the row renders popped out, and
    /// the hot-reload guards (`is_detached`) see it. Call it on a cold slot only — a `Spawned`
    /// one would be overwritten without `close()`.
    ///
    /// `has_secondary` marks the second pane too, declaring it (`secondary_spec`) when the
    /// rebuilt tab has none — a runtime (⌘D) split is in no config, so the rebuild is unsplit,
    /// and the returning pair still needs a slot to land in. With `false` a config split's
    /// declared second pane stays `Cold`, as `detach` leaves a secondary that wasn't live.
    /// Unknown tab = no-op (it left the config while out; `redock` drops it).
    pub fn mark_detached(&mut self, id: &str, has_secondary: bool) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        t.primary.slot = TabSlot::Detached;
        if has_secondary {
            let spec = secondary_spec(&t.primary.spec, id);
            t.secondary
                .get_or_insert(Pane::new(spec, TabSlot::Cold))
                .slot = TabSlot::Detached;
        }
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

    /// Test-only: give tab `id`'s main pane cold agent slots for `records`, `selected` among them,
    /// with no terminal in hand — so nothing can spawn against the null test window.
    #[cfg(test)]
    pub fn force_agents(&mut self, id: &str, records: &[&str], selected: Option<&str>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.primary.agents = records
                .iter()
                .map(|r| AgentSlot {
                    record_id: (*r).to_string(),
                    slot: TabSlot::Cold,
                    rex: RexState::Idle,
                })
                .collect();
            t.primary.selected = selected.map(str::to_string);
        }
    }

    /// Test-only: [`force_agents`](Self::force_agents) with each agent's terminal in hand, as an
    /// open leaves the ones not yet shown. Only safe on a tab that never goes on screen here.
    #[cfg(test)]
    pub fn force_ready_agents(&mut self, id: &str, records: &[&str], selected: Option<&str>) {
        self.force_agents(id, records, selected);
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            for a in &mut t.primary.agents {
                a.rex = RexState::Ready(PaneTarget {
                    endpoint: rex_client::Endpoint::Unix("/x.sock".into()),
                    session_id: "session:s".into(),
                    block_id: format!("block:{}", a.record_id),
                });
            }
        }
    }

    /// Test-only: agent `record_id`'s surface out in a pop-out, as `detach` leaves the pane.
    #[cfg(test)]
    pub fn force_detached_agent(&mut self, id: &str, record_id: &str) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.primary.slot = TabSlot::Detached;
            if let Some(a) = t.primary.agent_mut(record_id) {
                a.slot = TabSlot::Detached;
            }
        }
    }

    /// The agent tab `id`'s main pane has selected, on screen or not; `None` for a pane holding
    /// no agents. Pop-out records it as the agent whose surface travels.
    pub fn selected_agent(&self, id: &str) -> Option<&str> {
        let pane = &self.tabs.iter().find(|t| t.id == id)?.primary;
        pane.selected.as_deref().filter(|r| pane.agent(r).is_some())
    }

    /// The agent tab `id`'s main pane shows: its selected one, while the tab is the one on screen
    /// here. `activate` and `select_agent` show that agent's surface and hide the rest by it.
    pub fn visible_agent(&self, id: &str) -> Option<&str> {
        if self.active.as_deref() != Some(id) {
            return None;
        }
        let pane = &self.tabs.iter().find(|t| t.id == id)?.primary;
        if matches!(pane.slot, TabSlot::Detached) {
            return None;
        }
        pane.selected.as_deref().filter(|r| pane.agent(r).is_some())
    }

    /// The agent keys of tab `id`'s main pane ([`agent_key`]), in list order; empty for a tab
    /// holding no agents.
    #[cfg(test)]
    pub fn agent_keys(&self, id: &str) -> Vec<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| {
                t.primary
                    .agents
                    .iter()
                    .map(|a| agent_key(id, &a.record_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Show agent `record_id` in tab `id`'s main pane and hide its others. On the visible tab its
    /// surface spawns (attaching its terminal) if cold, is sized to the pane and takes focus when
    /// the main pane has it; off screen the choice waits for `activate`. Returns whether the tab
    /// has that agent; an unknown one changes nothing.
    pub fn select_agent(&mut self, id: &str, record_id: &str) -> Result<bool, SurfaceError> {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return Ok(false);
        };
        let pane = &mut self.tabs[idx].primary;
        if pane.agent(record_id).is_none() {
            return Ok(false);
        }
        pane.selected = Some(record_id.to_string());
        if self.active.as_deref() != Some(id) {
            return Ok(true);
        }
        let spawned = self.spawn_agent(idx, record_id);
        self.show_active();
        spawned.map(|_| true)
    }

    /// What a lifecycle op on tab `id`'s agent tabs needs to know of its main pane (as a user
    /// [`RexOpen`]), or `None` for a tab whose main pane holds no agent tabs.
    pub fn agent_request(&self, id: &str) -> Option<RexOpen> {
        let t = self.tabs.iter().find(|t| t.id == id)?;
        let spec = &t.primary.spec;
        if !hosts_agents(spec, PaneIdx::Primary) {
            return None;
        }
        Some(RexOpen {
            pane_key: spec.id.clone(),
            server: spec.server.clone()?,
            dir: spec.dir.clone(),
            startup: spec.startup.clone(),
            label: session_label(&t.title, PaneIdx::Primary),
            token: 0,
            auto: false,
            agents: true,
            resume: spec.resume.clone(),
        })
    }

    /// The active tab when its main pane holds agent tabs — where ⌘T, ⌘⇧T and Sleep Agent Tab act.
    pub fn active_agent_tab(&self) -> Option<&str> {
        let id = self.active.as_deref()?;
        self.agent_request(id).map(|_| id)
    }
    /// Follow a lifecycle op's answer in tab `id`'s main pane: one slot per live agent `found`
    /// lists, keeping each surface still on its terminal and closing the rest (one out in a pop-out
    /// stays), and `found.selected` selected. On the visible tab the selected agent spawns and
    /// shows. With nothing live the pane settles asleep, so only a user open asks again. A pane
    /// holding no agents that isn't on screen is left for its next open to reconcile.
    pub fn sync_agents(&mut self, id: &str, found: AgentsOpened) -> Result<(), SurfaceError> {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return Ok(());
        };
        let active = self.active.as_deref() == Some(id);
        let pane = &mut self.tabs[idx].primary;
        if !hosts_agents(&pane.spec, PaneIdx::Primary) || (pane.agents.is_empty() && !active) {
            return Ok(());
        }
        let mut old = std::mem::take(&mut pane.agents);
        for (record_id, target) in found.agents {
            let ready = RexState::Ready(target);
            let slot = match old.iter().position(|a| a.record_id == record_id) {
                Some(i) => {
                    let mut a = old.remove(i);
                    if a.rex != ready {
                        // A surface on another terminal (the agent was woken again) is stale.
                        if let TabSlot::Spawned(s) = std::mem::replace(&mut a.slot, TabSlot::Cold) {
                            s.close();
                        }
                        a.rex = ready;
                    }
                    a
                }
                None => AgentSlot {
                    record_id,
                    slot: TabSlot::Cold,
                    rex: ready,
                },
            };
            pane.agents.push(slot);
        }
        for a in old {
            match a.slot {
                TabSlot::Spawned(s) => s.close(),
                TabSlot::Detached => pane.agents.push(a),
                TabSlot::Cold => {}
            }
        }
        if let Some(selected) = found.selected {
            pane.selected = Some(selected);
        }
        if pane.agents.is_empty() {
            pane.selected = None;
            pane.rex = RexState::Asleep;
        } else if pane.rex.opening().is_none() {
            pane.rex = RexState::Idle;
        }
        if !active {
            return Ok(());
        }
        let spawned = match self.tabs[idx].primary.selected.clone() {
            Some(r) => self.spawn_agent(idx, &r).map(|_| ()),
            None => Ok(()),
        };
        self.show_active();
        spawned
    }

    /// Close agent `record_id`'s surface in tab `id`'s main pane ahead of its window closing (sleep
    /// or close), so the relay's end isn't taken for the agent exiting. Its slot stays, cold, for
    /// the op's answer to settle ([`sync_agents`](Self::sync_agents)).
    pub fn release_agent(&mut self, id: &str, record_id: &str) {
        let Some(a) = self
            .tabs
            .iter_mut()
            .find(|t| t.id == id)
            .and_then(|t| t.primary.agent_mut(record_id))
        else {
            return;
        };
        if matches!(a.slot, TabSlot::Spawned(_)) {
            if let TabSlot::Spawned(s) = std::mem::replace(&mut a.slot, TabSlot::Cold) {
                s.close();
            }
        }
    }

    /// Snapshot every tab for the chrome.
    ///
    /// `warn` is **derived here, per render** — deliberately not cached on the entry.
    /// A tab's directory can appear or vanish long after the tab is materialized, and
    /// `set_meta` relabels a kept tab without touching a cached flag, so a cached ⚠
    /// would outlive the directory arriving. The filesystem is the one
    /// source of truth for "does this dir exist"; don't reintroduce a copy. The cost is
    /// one `stat` per tab, and this runs per sidebar render (window init / reconcile /
    /// activate), not per frame. A remote Rex tab's dir is a path on its server, so it isn't
    /// checked here. A failed Rex open's reason outranks the dir check.
    ///
    /// A Rex tab always offers ☠ (it destroys the tab's Rex sessions) and a presence dot (from
    /// Rex, not its `probe`).
    pub fn tab_dtos(&self) -> Vec<TabDto> {
        self.tabs
            .iter()
            .map(|t| TabDto {
                id: t.id.clone(),
                title: t.title.clone(),
                warn: t.rex_error.clone().or_else(|| {
                    let spec = &t.primary.spec;
                    let unclaimed = crate::rex::unclaimed::is_unclaimed(
                        &spec.id,
                        &spec.dir,
                        spec.server.as_deref(),
                    );
                    (!is_remote(spec) && !unclaimed && !spec.dir.exists())
                        .then(|| format!("directory not found: {}", spec.dir.display()))
                }),
                spawned: t.primary.live(),
                group: t.primary.spec.group.clone(),
                has_probe: t.primary.spec.probe.is_some() || t.primary.spec.server.is_some(),
                has_kill: t.primary.spec.kill.is_some() || t.primary.spec.server.is_some(),
                has_suspend: suspend_cmd(&t.primary.spec).is_some(),
                has_cmd: start_cmd(&t.primary.spec).is_some(),
                tree: t.primary.spec.tree,
                tree_path: t.primary.spec.tree_path.clone(),
                detached: matches!(t.primary.slot, TabSlot::Detached),
                // Read straight off the `TabEntry` already in hand: re-finding `t` in `self.tabs`
                // by id for every row would turn this map into an O(n²) pass.
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
                agents: hosts_agents(&t.primary.spec, PaneIdx::Primary),
                presence: None, // filled by the manager's PresenceCache, not the Registry
            })
            .collect()
    }

    /// `(id, dir, title, probe_cmd)` for every tab with a configured probe — the
    /// work-list the probe runner snapshots. Includes cold tabs (a session can
    /// exist while warden's own surface is unloaded — that's the point). Excludes Rex tabs:
    /// Rex reports their sessions (`rex_tabs`).
    pub fn probe_targets(&self) -> Vec<ProbeTarget> {
        self.tabs
            .iter()
            .filter(|t| runs_probe(&t.primary.spec))
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

    /// How to end tab `id`'s session the `how` way — its `suspend` or `kill` command — else `None`
    /// (unknown tab, that command isn't set, or a suspend on a remote Rex tab — `suspend_cmd`).
    /// A Rex tab's destroy is its panes' Rex sessions —
    /// both pane keys, mapped or not, since a split's session outlives the split. Independent of
    /// the surface being live — a session can exist while warden's surface is cold.
    pub fn end_target(&self, id: &str, how: EndSession) -> Option<EndTarget> {
        let t = self.tabs.iter().find(|t| t.id == id)?;
        let spec = &t.primary.spec;
        let cmd = match how {
            EndSession::Destroy if spec.server.is_some() => {
                return Some(EndTarget::Rex(vec![
                    spec.id.clone(),
                    secondary_spec(spec, id).id,
                ]));
            }
            EndSession::Suspend => suspend_cmd(spec),
            EndSession::Destroy => spec.kill.as_ref(),
        };
        cmd.map(|c| EndTarget::Command(spec.dir.clone(), t.title.clone(), c.clone()))
    }

    /// Restart tab `id`'s session by typing its startup command into the **live** shell and
    /// submitting it (`TerminalSurface::run_command` — text inject + a real Enter keypress) — the
    /// runtime twin of how the tab launched. Preserves the terminal/scrollback (no respawn). Returns
    /// `false` (no-op) when the tab is unknown, has no startup command (`start_cmd`), or is cold — a cold tab has
    /// no shell to type into and is (re)started by activating it. The chrome only offers this on a
    /// live+startable tab with an absent session, so `false` means a stale/racing click.
    pub fn start_session(&self, id: &str) -> bool {
        let Some(t) = self.tabs.iter().find(|t| t.id == id) else {
            return false;
        };
        let Some(cmd) = start_cmd(&t.primary.spec) else {
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

    /// Apply a kept tab's in-place metadata (`TabMeta`, plus its derived `tree`/`tree_path`)
    /// from a hot-reload — NEVER the surface/PTY, so the edit takes effect live without respawn.
    /// Which changes arrive here: `warden_config::reconcile`. No-op if `id` is unknown.
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
            t.primary.spec.suspend = meta.suspend.clone();
            t.primary.spec.resume = meta.resume.clone();
            t.primary.spec.split = meta.split.clone();
            // Tree-ness can flip on a kept tab when its group moves between a root
            // section and a plain group (curated↔discovered shadowing) — recomputed
            // upstream in `reconcile_ops`, so it must be applied here too, not left
            // stale from the prior render.
            t.primary.spec.tree = tree;
            t.primary.spec.tree_path = tree_path;
        }
    }

    /// Which side tab `id`'s CONFIG split puts its second pane on, or `None` for an unknown
    /// tab, an unsplit one, or a runtime (⌘D) split — that one has no `side` and always sits
    /// on the right, exactly as the docked chrome draws it (`.secondary-left` keys off
    /// `split_layout`, which a runtime split never carries). Pop-out reads this to decide
    /// whether the detached window's holes are mirrored (`PaneIdx::hole`).
    pub fn split_side(&self, id: &str) -> Option<warden_config::SplitSide> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.primary.spec.split.as_ref())
            .map(|s| s.side)
    }

    /// Ids of the tabs with a terminal standing — primary live here, or `Detached` into a
    /// popped-out window — in sidebar order. What `remember_tabs` records as "loaded".
    pub fn loaded_ids(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter(|t| t.primary.live() || matches!(t.primary.slot, TabSlot::Detached))
            .map(|t| t.id.clone())
            .collect()
    }

    /// Tab `id`'s (primary pane's) spec, or `None` if unknown. Pop-out records it as the spec
    /// the travelling surface runs (`DetachedSurface::spec`), so `redock` can tell whether the
    /// config it returns to still matches.
    pub fn spec_of(&self, id: &str) -> Option<&TabSpec> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| &t.primary.spec)
    }

    /// The display title of tab `id` (for a detached window's banner), or `None` if unknown.
    pub fn tab_title(&self, id: &str) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
    }

    /// Map a surface handle back to (tab id, which pane). Every pane and every agent of a main
    /// pane is searched — a secondary's or an agent's bell, notification or child-exit is
    /// otherwise silently unroutable, which is exactly how a split tab would stop badging.
    pub fn locate_surface(&self, surface_id: usize) -> Option<(String, PaneIdx)> {
        let (tab, which, _) = self.locate_key(&self.surface_key(surface_id)?)?;
        Some((tab, which))
    }

    /// The key of the live surface `surface_id`: its pane's spec id (the tab key for a main pane,
    /// `secondary_id` for a split) or, for an agent's surface, its [`agent_key`]. `None` if it
    /// isn't in this registry.
    pub fn surface_key(&self, surface_id: usize) -> Option<String> {
        let is = |slot: &TabSlot| matches!(slot, TabSlot::Spawned(s) if s.id() == surface_id);
        self.tabs.iter().find_map(|t| {
            t.panes().find_map(|p| {
                if is(&p.slot) {
                    return Some(p.spec.id.clone());
                }
                let a = p.agents.iter().find(|a| is(&a.slot))?;
                Some(agent_key(&t.id, &a.record_id))
            })
        })
    }

    /// The tab, pane and — for an agent's key — agent record that key `key` names here.
    pub fn locate_key(&self, key: &str) -> Option<(String, PaneIdx, Option<String>)> {
        self.tabs.iter().find_map(|t| {
            t.panes_indexed().find_map(|(which, p)| {
                if p.spec.id == key {
                    return Some((t.id.clone(), which, None));
                }
                let a = p
                    .agents
                    .iter()
                    .find(|a| agent_key(&t.id, &a.record_id) == key)?;
                Some((t.id.clone(), which, Some(a.record_id.clone())))
            })
        })
    }

    /// The Rex pane key of live surface `surface_id` ([`surface_key`](Self::surface_key)): what
    /// its size claims and exit check name. `None` for a plain tab's surface.
    pub fn rex_key_of_surface(&self, surface_id: usize) -> Option<String> {
        let key = self.surface_key(surface_id)?;
        let (tab, _, _) = self.locate_key(&key)?;
        self.spec_of(&tab)?.server.as_ref()?;
        Some(key)
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
    ///
    /// A Rex pane (its spec names a `server`) needs its session first, and finding that is
    /// network work this (main) thread never waits on: a cold Rex pane with no session in hand
    /// asks the window's [`RexOpener`] for one and stays cold (`Ok(false)`) until
    /// [`rex_opened`](Self::rex_opened) lands the answer and spawns it. One open per pane is in
    /// flight at a time. With a session in hand it spawns `warden-relay` on it
    /// (`rex::spawn::effective_spec`), using the session up — the next spawn asks again, so an
    /// unload/relaunch reattaches whatever the session is by then.
    ///
    /// A main pane holding agents ([`hosts_agents`]) asks once for all of them
    /// (`RexOpen.agents`) and then spawns only its selected agent's surface. `auto` says warden,
    /// not the user, is opening it: an automatic open never wakes an agent, so after one that found
    /// nothing live (`RexState::Asleep`) only a user open asks again.
    fn ensure_spawned(
        &mut self,
        idx: usize,
        which: PaneIdx,
        auto: bool,
    ) -> Result<bool, SurfaceError> {
        let agents = hosts_agents(&self.tabs[idx].primary.spec, which);
        let tab = &mut self.tabs[idx];
        let label = session_label(&tab.title, which);
        let Some(pane) = tab.pane_mut(which) else {
            return Ok(false);
        };
        if !matches!(pane.slot, TabSlot::Cold) {
            return Ok(false);
        }
        if agents && !pane.agents.is_empty() {
            let shown = pane
                .selected
                .clone()
                .filter(|r| pane.agent(r).is_some())
                .or_else(|| pane.agents.first().map(|a| a.record_id.clone()));
            pane.selected = shown.clone();
            return match shown {
                Some(r) => self.spawn_agent(idx, &r),
                None => Ok(false),
            };
        }
        let rex = std::mem::replace(&mut pane.rex, RexState::Idle);
        let spec = match (pane.spec.server.clone(), rex) {
            (None, _) => pane.spec.clone(),
            (Some(_), RexState::Ready(target)) => crate::rex::spawn::effective_spec(
                &pane.spec,
                &target,
                self.report_socket.as_deref(),
            ),
            (Some(_), RexState::Opening { token, wake }) => {
                pane.rex = RexState::Opening {
                    token,
                    wake: wake || (agents && !auto),
                };
                return Ok(false);
            }
            (Some(_), RexState::Asleep) if auto => {
                pane.rex = RexState::Asleep;
                return Ok(false);
            }
            (Some(server), RexState::Idle | RexState::Asleep) => {
                let Some(open) = &self.rex_opener else {
                    tab.rex_error = Some(format!("Rex server {server}: no connection to Rex"));
                    return Ok(false);
                };
                let token = self.next_rex_token;
                self.next_rex_token += 1;
                let started = open(RexOpen {
                    pane_key: pane.spec.id.clone(),
                    server,
                    dir: pane.spec.dir.clone(),
                    startup: pane.spec.startup.clone(),
                    label,
                    token,
                    auto,
                    agents,
                    resume: pane.spec.resume.clone(),
                });
                match started {
                    Ok(()) => pane.rex = RexState::Opening { token, wake: false },
                    Err(reason) => tab.rex_error = Some(reason),
                }
                return Ok(false);
            }
        };
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

    /// Spawn agent `record_id`'s surface in tab `idx`'s main pane if it is cold with its terminal
    /// in hand: a relay on that agent's block, under its [`agent_key`], born at the main pane's
    /// rect. Returns whether it spawned this call.
    fn spawn_agent(&mut self, idx: usize, record_id: &str) -> Result<bool, SurfaceError> {
        let t = &self.tabs[idx];
        let Some(a) = t.primary.agent(record_id) else {
            return Ok(false);
        };
        let (TabSlot::Cold, RexState::Ready(target)) = (&a.slot, &a.rex) else {
            return Ok(false);
        };
        let mut spec = t.primary.spec.clone();
        spec.id = agent_key(&t.id, record_id);
        let spec = crate::rex::spawn::effective_spec(&spec, target, self.report_socket.as_deref());
        let s = GhosttySurface::new(self.ns_window, self.rect_for(PaneIdx::Primary), &spec)?;
        match self.tabs[idx].primary.agent_mut(record_id) {
            Some(a) => a.slot = TabSlot::Spawned(s),
            None => s.close(),
        }
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
            // An open still in flight is no longer wanted: its answer lands stale.
            pane.rex = RexState::Idle;
            // A `Detached` slot keeps its placeholder: its surface lives in another window's
            // registry (a pop-out), and a `Cold` one would look locally respawnable, spawning a
            // second surface for the same identity.
            any_closed |= pane.close_live();
            // The agents go with their surfaces; the next open reconciles them afresh. One out in
            // a pop-out keeps its slot to return to.
            pane.agents.retain(|a| matches!(a.slot, TabSlot::Detached));
            if pane.agents.is_empty() {
                pane.selected = None;
            }
        }
        // A config split whose shell exited comes back on relaunch — this is the relaunch. Declared
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

    /// Land the answer to the Rex open `token` for pane `pane_key` (main thread). Stale unless
    /// that pane is still waiting on exactly that open. A failure leaves the pane cold with the
    /// reason on its tab's `warn`; the next activate asks again. A session brings the pane live:
    /// the visible tab is re-activated (shown and focused, as a click would have), any other is
    /// spawned hidden, and either way a Rex primary that just went live brings its declared
    /// split up after it, as `activate` would.
    ///
    /// An agent pane's answer gives it one slot per live agent and spawns only the selected one's
    /// surface. With nothing live the pane stays cold and settles `Asleep`: no automatic open asks
    /// again, and nothing is put on screen.
    pub fn rex_opened(
        &mut self,
        pane_key: &str,
        token: u64,
        result: Result<RexAnswer, String>,
    ) -> RexLanded {
        let Some((idx, which)) = self.tabs.iter().enumerate().find_map(|(i, t)| {
            t.panes_indexed()
                .find(|(_, p)| p.spec.id == pane_key && p.rex.opening() == Some(token))
                .map(|(which, _)| (i, which))
        }) else {
            return RexLanded::Stale;
        };
        let tab = &mut self.tabs[idx];
        let Some(pane) = tab.pane_mut(which) else {
            return RexLanded::Stale;
        };
        let wake = matches!(pane.rex, RexState::Opening { wake: true, .. });
        match result {
            Err(reason) => {
                pane.rex = RexState::Idle;
                tab.rex_error = Some(reason);
                RexLanded::Failed
            }
            Ok(answer) => {
                match answer {
                    RexAnswer::Pane(target) => pane.rex = RexState::Ready(target),
                    RexAnswer::Agents(found) => {
                        pane.agents = found
                            .agents
                            .into_iter()
                            .map(|(record_id, target)| AgentSlot {
                                record_id,
                                slot: TabSlot::Cold,
                                rex: RexState::Ready(target),
                            })
                            .collect();
                        pane.selected = found.selected;
                        if pane.agents.is_empty() {
                            pane.rex = RexState::Asleep;
                            tab.rex_error = None;
                            if wake {
                                // The user opened it while warden's open was in flight.
                                return RexLanded::Opened(
                                    self.ensure_spawned(idx, which, false).map(|_| ()),
                                );
                            }
                            return RexLanded::Opened(Ok(()));
                        }
                        pane.rex = RexState::Idle;
                    }
                }
                tab.rex_error = None;
                let id = tab.id.clone();
                if self.active.as_deref() == Some(id.as_str()) {
                    return RexLanded::Opened(self.activate(&id).map(|_| ()));
                }
                // A secondary spawns only beside a primary live in this registry — the gate
                // `activate` and `ensure_spawned_by_id` share. Without it, it waits `Ready`.
                let primary_live = |r: &Self| r.tabs[idx].primary.live();
                let spawned = if which == PaneIdx::Primary || primary_live(self) {
                    self.ensure_spawned(idx, which, true).map(|_| ())
                } else {
                    Ok(())
                };
                if primary_live(self) {
                    let _ = self.ensure_spawned(idx, PaneIdx::Secondary, true);
                }
                for p in self.tabs[idx].panes() {
                    p.surfaces().for_each(|s| s.hide());
                }
                RexLanded::Opened(spawned)
            }
        }
    }

    /// Whether tab `id` is waiting on a Rex open for its primary pane — cold, with nothing on
    /// screen yet, though activated.
    pub fn is_connecting(&self, id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.id == id && t.primary.rex.opening().is_some())
    }

    /// The Rex pane key of tab `id`'s pane `which` (its `TabSpec.id`), or `None` when that pane
    /// doesn't exist or isn't a Rex pane.
    #[cfg(test)]
    pub fn rex_pane_key(&self, id: &str, which: PaneIdx) -> Option<String> {
        let t = self.tabs.iter().find(|t| t.id == id)?;
        t.pane(which)?;
        rex_pane_key(&t.primary.spec, which)
    }

    /// `(tab id, server)` for every Rex tab, in sidebar order — whose presence Rex reports.
    pub fn rex_tabs(&self) -> Vec<(String, String)> {
        self.tabs
            .iter()
            .filter_map(|t| Some((t.id.clone(), t.primary.spec.server.clone()?)))
            .collect()
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
            .map(|t| t.panes().any(Pane::live))
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
    /// untouched. Returns what `unload` would (`Cold`): the live neighbour the sidebar leans to
    /// when this was the active tab, else `None` — the same "this tab's terminal is gone" outcome,
    /// reached from another window. A main pane whose travelling agent ended but which still holds
    /// a running sibling is not gone (`SiblingShown`): it stays where it is, selecting that
    /// sibling, for the caller to activate when it is the tab on screen.
    pub fn clear_detached(&mut self, id: &str) -> DetachedEnded {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return DetachedEnded::Cold(None);
        };
        let mut any = false;
        let mut sibling = false;
        for pane in self.tabs[idx].panes_mut() {
            if matches!(pane.slot, TabSlot::Detached) {
                pane.slot = TabSlot::Cold;
                any = true;
            }
            // The agent that went out with the pane ended there. A selection naming it leans to
            // the nearest running sibling; one the user made here since stays.
            if let Some(at) = pane
                .agents
                .iter()
                .position(|a| matches!(a.slot, TabSlot::Detached))
            {
                let gone = pane.agents[at].record_id.clone();
                if pane.selected.as_deref() == Some(gone.as_str()) {
                    let running: Vec<bool> = pane
                        .agents
                        .iter()
                        .enumerate()
                        .map(|(i, a)| i != at && matches!(a.rex, RexState::Ready(_)))
                        .collect();
                    let next = shell_core::pick_live_neighbour(at, &running);
                    pane.selected = next.map(|n| pane.agents[n].record_id.clone());
                }
                pane.agents.remove(at);
                sibling |= pane.selected.as_deref().is_some_and(|s| {
                    pane.agent(s)
                        .is_some_and(|a| matches!(a.rex, RexState::Ready(_)))
                });
            }
        }
        if !any {
            return DetachedEnded::Cold(None);
        }
        if sibling {
            return DetachedEnded::SiblingShown;
        }
        if self.active.as_deref() == Some(id) {
            return DetachedEnded::Cold(self.lean_to_live_neighbour(idx));
        }
        DetachedEnded::Cold(None)
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
    ///
    /// A main pane holding agents carries its selected agent's surface out, leaving that agent's
    /// slot and the pane's own `Detached`; its other agents stay here, hidden, until it returns.
    pub fn detach(&mut self, id: &str) -> Option<(GhosttySurface, Option<GhosttySurface>)> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        let main = &mut self.tabs[idx].primary;
        let shown = main
            .selected
            .as_deref()
            .and_then(|r| main.agents.iter().position(|a| a.record_id == r));
        let primary = match shown {
            Some(i) => {
                if !matches!(main.agents[i].slot, TabSlot::Spawned(_)) {
                    return None;
                }
                let TabSlot::Spawned(s) =
                    std::mem::replace(&mut main.agents[i].slot, TabSlot::Detached)
                else {
                    return None;
                };
                main.slot = TabSlot::Detached;
                s
            }
            None if !main.agents.is_empty() => return None,
            None => match std::mem::replace(&mut main.slot, TabSlot::Detached) {
                TabSlot::Spawned(s) => s,
                TabSlot::Cold => {
                    main.slot = TabSlot::Cold;
                    return None;
                }
                TabSlot::Detached => {
                    main.slot = TabSlot::Detached;
                    return None;
                }
            },
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
    ///
    /// Warden, not the user, is bringing the tab up (`load_on_open` turned on, a pop-out), so an
    /// agent pane opens automatically: it wakes nothing.
    pub fn ensure_spawned_by_id(&mut self, id: &str) -> Result<(), SurfaceError> {
        if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
            self.ensure_spawned(idx, PaneIdx::Primary, true)?;
            if self.tabs[idx].primary.live() && self.tabs[idx].secondary.is_some() {
                let _ = self.ensure_spawned(idx, PaneIdx::Secondary, true);
            }
        }
        Ok(())
    }

    /// Return a surface to slot `id` as `Spawned` — the inverse of `detach`,
    /// called when a popped-out window's tab is reparented back into its
    /// origin registry. Succeeds from a `Detached` **or** a `Cold` slot →
    /// `Ok(())`, and the slot becomes `Spawned(surface)`.
    ///
    /// The slot is normally `Detached`: the placeholder `detach` left when the
    /// origin stayed open, or the one `mark_detached` stood up when the origin
    /// was rebuilt while the tab was out. A `Cold` slot is accepted too — it
    /// holds no surface, so landing the returning one there leaks nothing.
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
    /// (`secondary_spec`, the same derivation `split` and `mark_detached` use) rather than
    /// refusing: refusing would end a live PTY purely because its slot is missing.
    ///
    /// A returning `agent`'s surface (the record `selected_agent` named at pop-out) lands in that
    /// agent's slot — the one `detach` left `Detached`, or, in a pane rebuilt while it was out, a
    /// slot made for it — and is selected, so its key stays the agent's for size and exit.
    pub fn attach(
        &mut self,
        id: &str,
        primary: GhosttySurface,
        secondary: Option<GhosttySurface>,
        agent: Option<&str>,
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
        let agent = agent.filter(|_| hosts_agents(&t.primary.spec, PaneIdx::Primary));
        if agent.is_some_and(|r| {
            t.primary
                .agent(r)
                .is_some_and(|a| matches!(a.slot, TabSlot::Spawned(_)))
        }) {
            return Err((primary, secondary));
        }
        match agent {
            Some(record_id) => {
                let main = &mut t.primary;
                match main.agent_mut(record_id) {
                    Some(a) => a.slot = TabSlot::Spawned(primary),
                    None => main.agents.push(AgentSlot {
                        record_id: record_id.to_string(),
                        slot: TabSlot::Spawned(primary),
                        rex: RexState::Idle,
                    }),
                }
                main.selected = Some(record_id.to_string());
                main.slot = TabSlot::Cold;
            }
            None => t.primary.slot = TabSlot::Spawned(primary),
        }
        match secondary {
            Some(s) => match t.secondary.as_mut() {
                Some(p) => p.slot = TabSlot::Spawned(s),
                None => {
                    let spec = secondary_spec(&t.primary.spec, id);
                    t.secondary = Some(Pane::new(spec, TabSlot::Spawned(s)));
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

    /// Show + focus tab `id` (spawning it if cold) and hide the rest. Returns `Ok(true)` iff this call
    /// spawned a **fresh** surface (the tab was cold) — so the caller can arm a session-start await
    /// only when `initial_input` actually ran; `Ok(false)` for a warm tab-switch. An unknown id is a
    /// no-op `Ok(false)`.
    ///
    /// If the lazy spawn fails the tab is marked active anyway but stays cold, so the hole shows
    /// the blank placeholder; the error is returned for the caller to surface, and re-activating
    /// retries.
    ///
    /// Warden itself shows the tab (a window's first tab, a hot reload, a landed open), so an
    /// agent pane opens automatically and wakes nothing; [`activate_user`](Self::activate_user) is
    /// the user selecting it.
    pub fn activate(&mut self, id: &str) -> Result<bool, SurfaceError> {
        self.activate_as(id, true)
    }

    /// [`activate`](Self::activate) for the user selecting tab `id`: an agent pane with nothing
    /// live asks the hub to wake its last-selected agent.
    pub fn activate_user(&mut self, id: &str) -> Result<bool, SurfaceError> {
        self.activate_as(id, false)
    }

    fn activate_as(&mut self, id: &str, auto: bool) -> Result<bool, SurfaceError> {
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
        let spawned = self.ensure_spawned(idx, PaneIdx::Primary, auto);
        if self.tabs[idx].primary.live() && self.tabs[idx].secondary.is_some() {
            let _ = self.ensure_spawned(idx, PaneIdx::Secondary, auto);
        }
        self.active = Some(id.to_string());
        self.show_active();
        spawned
    }

    /// Put the active tab on screen and every other surface away: each of its panes shows its one
    /// surface — a main pane holding agents, its [`visible_agent`](Self::visible_agent)'s — sized
    /// to that pane's hole, and hides the rest.
    fn show_active(&self) {
        for t in &self.tabs {
            let on_screen = self.active.as_deref() == Some(t.id.as_str());
            for (which, pane) in t.panes_indexed() {
                let shown = match self
                    .visible_agent(&t.id)
                    .filter(|_| which == PaneIdx::Primary)
                {
                    Some(r) => match pane.agent(r).map(|a| &a.slot) {
                        Some(TabSlot::Spawned(s)) => Some(s),
                        _ => None,
                    },
                    None if on_screen && pane.agents.is_empty() => pane.shown(),
                    None => None,
                };
                for s in pane.surfaces() {
                    if shown.is_some_and(|v| std::ptr::eq(v, s)) {
                        // Each pane gets ITS OWN hole's last-reported rect, not a shared
                        // one — see `rect_for`'s doc comment. Applying the primary's rect
                        // to the secondary here would immediately undo the size
                        // `ensure_spawned` just gave a freshly-spawned secondary above.
                        s.set_frame(self.rect_for(which));
                        s.show();
                        // Show every pane of the active tab, but focus only the one
                        // `focused` names — focusing each in turn would hand focus to
                        // whichever pane iterates last, by construction order.
                        if which == t.focused {
                            s.focus();
                        }
                    } else {
                        s.hide();
                    }
                }
            }
        }
    }

    /// Apply a hole's rect to the pane that occupies it, and remember it for later spawns.
    /// Returns the pane's new size when it is a live Rex pane, for `rex::size` to send while
    /// warden owns its block.
    pub fn set_pane_frame(&mut self, which: PaneIdx, rect: PixelRect) -> Option<PaneSize> {
        match which {
            PaneIdx::Primary => self.last_rect = rect,
            PaneIdx::Secondary => self.last_rect_secondary = rect,
        }
        let active = self.active.clone()?;
        let t = self.tabs.iter().find(|t| t.id == active)?;
        let pane = t.pane(which)?;
        let s = pane.shown()?;
        s.set_frame(rect);
        let key = match pane.selected.as_deref().filter(|_| !pane.agents.is_empty()) {
            Some(agent) => agent_key(&t.id, agent),
            None => rex_pane_key(&t.primary.spec, which)?,
        };
        PaneSize::of(key, s)
    }

    /// The size of the live Rex pane surface `surface_id` shows, or `None` when it shows a plain
    /// pane or isn't in this registry.
    pub fn rex_size_of_surface(&self, surface_id: usize) -> Option<PaneSize> {
        let key = self.rex_key_of_surface(surface_id)?;
        let s = self.tabs.iter().find_map(|t| {
            t.panes()
                .find_map(|p| p.surfaces().find(|s| s.id() == surface_id))
        })?;
        PaneSize::of(key, s)
    }

    /// Agent `record_id`'s relay in tab `id`'s main pane exited: close its surface and drop its
    /// slot (its window is let go on a worker, `rex::spawn::end_if_exited`). When it was the
    /// selected one the pane selects its nearest running agent — live, or with its terminal in
    /// hand — and, on screen, spawns and shows it; with none running the pane goes cold as
    /// [`unload`](Self::unload) leaves it. `None` for an unknown agent.
    pub fn close_agent(&mut self, id: &str, record_id: &str) -> Option<AgentClosed> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        let pane = &mut self.tabs[idx].primary;
        let at = pane.agents.iter().position(|a| a.record_id == record_id)?;
        // Every agent but this one that can be shown, in list order: live, or still running in
        // Rex with its terminal in hand (only the selected agent ever has a surface).
        let showable: Vec<bool> = pane
            .agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                i != at
                    && matches!(
                        (&a.slot, &a.rex),
                        (TabSlot::Spawned(_), _) | (TabSlot::Cold, RexState::Ready(_))
                    )
            })
            .collect();
        if !showable.contains(&true) {
            // The pane's last running agent: it ends as the tab's does.
            return Some(AgentClosed::PaneCold(self.unload(id)));
        }
        if pane.selected.as_deref() == Some(record_id) {
            pane.selected = shell_core::pick_live_neighbour(at, &showable)
                .map(|n| pane.agents[n].record_id.clone());
        }
        if let TabSlot::Spawned(s) = pane.agents.remove(at).slot {
            s.close();
        }
        if self.active.as_deref() == Some(id) {
            if let Some(next) = self.tabs[idx].primary.selected.clone() {
                if let Err(e) = self.spawn_agent(idx, &next) {
                    eprintln!("warden: couldn't open agent {next:?} of tab {id:?}: {e}");
                }
            }
            if !self.tabs[idx].primary.live() {
                // Its surface is already closed, so `unload` finds nothing to close and leaves
                // the sidebar where it is; lean from here, as it would have.
                let next = self.unload(id);
                let next = match next {
                    None if self.active.as_deref() == Some(id) => self.lean_to_live_neighbour(idx),
                    next => next,
                };
                return Some(AgentClosed::PaneCold(next));
            }
            self.show_active();
        }
        Some(AgentClosed::PaneLive)
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
                pane.close_live();
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
                pane.close_live();
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
            suspend: None,
            tree: false,
            tree_path: Vec::new(),
            split: None,
            server: None,
            resume: None,
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
            suspend: None,
            tree: false,
            tree_path: Vec::new(),
            split: None,
            server: None,
            resume: None,
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
        assert!(r.tab_dtos()[0].warn.is_some(), "missing dir must set warn");
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
        assert!(r.tab_dtos()[0].warn.is_some(), "absent at add time → warn");

        std::fs::create_dir(&dir).unwrap();
        assert!(
            r.tab_dtos()[0].warn.is_none(),
            "dir appeared after add → warn must clear without re-adding the tab"
        );

        std::fs::remove_dir(&dir).unwrap();
        assert!(
            r.tab_dtos()[0].warn.is_some(),
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
                suspend: None,
                resume: None,
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
                suspend: None,
                resume: None,
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

    // --- detach / attach (pop a tab out) -------------------------------------
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
    // accepting one back — that needs a real surface, so it's covered by the
    // by-eye checks in docs/native-splits-direction.md instead.

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
    fn mark_detached_makes_a_rebuilt_tab_read_as_popped_out() {
        // An origin window rebuilt while one of its tabs is still popped out: the tab's live
        // terminal is in the detached window, so the rebuilt row must be the same `Detached`
        // placeholder `detach` leaves — never a cold slot `activate` would spawn a second
        // terminal into.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false).unwrap();
        r.mark_detached("t0", false);
        assert!(r.is_detached("t0"));
        assert!(r.activate("t0").is_ok());
        assert!(r.ensure_spawned_by_id("t0").is_ok());
        assert!(!r.is_spawned("t0"), "a popped-out tab never spawns locally");
        assert!(
            r.tab_dtos()[0].detached,
            "the row renders popped out, not cold"
        );
        assert!(!r.is_split("t0"));
    }

    #[test]
    fn mark_detached_with_a_secondary_declares_a_detached_second_pane() {
        // A runtime (⌘D) split lives in no config, so the rebuilt origin has no second pane; the
        // returning pair still needs one to land in, and the row reads as split meanwhile.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("t0", "/tmp"), false).unwrap();
        r.mark_detached("t0", true);
        assert!(r.is_pane_detached("t0", PaneIdx::Primary));
        assert!(r.is_pane_detached("t0", PaneIdx::Secondary));
        assert_eq!(
            r.secondary_spec_for_test("t0").map(|s| s.id),
            Some("t0::2".to_string())
        );
    }

    #[test]
    fn mark_detached_of_unknown_tab_is_a_noop() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.mark_detached("nope", true);
        assert!(r.tab_dtos().is_empty());
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
    fn end_flags_reflect_spec_independently() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s_kill = spec("t0", "/tmp");
        s_kill.kill = Some("kill-cmd {dir}".into());
        let mut s_suspend = spec("t1", "/tmp");
        s_suspend.suspend = Some("suspend-cmd".into());
        let _ = r.add(&s_kill, false);
        let _ = r.add(&s_suspend, false);
        let _ = r.add(&spec("t2", "/tmp"), false); // neither
        let d = r.tab_dtos();
        assert!(d[0].has_kill && !d[0].has_suspend);
        assert!(!d[1].has_kill && d[1].has_suspend);
        assert!(!d[2].has_kill && !d[2].has_suspend);
    }

    #[test]
    fn end_target_picks_the_command_for_how() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s = spec("t0", "/tmp/a");
        s.kill = Some("kill {title}".into());
        s.suspend = Some("suspend {title}".into());
        let _ = r.add(&s, false);
        let mut only_kill = spec("t1", "/tmp/b");
        only_kill.kill = Some("kill".into());
        let _ = r.add(&only_kill, false);
        let at = |cmd: &str| {
            Some(EndTarget::Command(
                std::path::PathBuf::from("/tmp/a"),
                "t0".to_string(),
                cmd.to_string(),
            ))
        };
        assert_eq!(r.end_target("t0", EndSession::Destroy), at("kill {title}"));
        assert_eq!(
            r.end_target("t0", EndSession::Suspend),
            at("suspend {title}")
        );
        assert_eq!(r.end_target("t1", EndSession::Suspend), None); // no suspend command
        assert_eq!(r.end_target("nope", EndSession::Destroy), None); // unknown id
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

    // --- split / close_secondary / focus_pane ---------------------------------

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
        // is what "a closed pane holds only until the tab is relaunched" means structurally.
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
                suspend: None,
                resume: None,
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
    // `activate` focuses only the pane `t.focused` names, by iterating
    // `TabEntry::panes_indexed()` and comparing each pane's `PaneIdx` against it.
    //
    // That cannot be exercised through `Registry::activate` itself in this file's unit
    // tests: every test here runs against `ns_window = null` so panes never leave `Cold`
    // (see the detach/attach block earlier in this file), and `activate` on a COLD tab
    // SIGSEGVs — `GhosttySurface::new`'s null-window path is not the "safely returns Err"
    // case it looks like — so tests here call `activate` only on `Detached` tabs, which
    // spawn nothing. So this test pins the one ingredient that
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

    // --- per-pane geometry ------------------------------------------------------

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
        // `last_rect`/`last_rect_secondary` are two fields; this pins that they don't alias.
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

    // --- surface→tab routing ------------------------------------------------------

    #[test]
    fn pane_index_round_trips_and_floors_to_primary() {
        assert_eq!(PaneIdx::Primary.index(), 0);
        assert_eq!(PaneIdx::Secondary.index(), 1);
        assert_eq!(PaneIdx::from_index(0), PaneIdx::Primary);
        assert_eq!(PaneIdx::from_index(1), PaneIdx::Secondary);
        assert_eq!(PaneIdx::from_index(7), PaneIdx::Primary, "never an error");
    }

    #[test]
    fn hole_mapping_is_identity_unless_mirrored() {
        // Unmirrored (secondary on the right, or one hole): hole i IS pane i.
        assert_eq!(PaneIdx::Primary.hole(false), 0);
        assert_eq!(PaneIdx::Secondary.hole(false), 1);
        assert_eq!(PaneIdx::from_hole(0, false), PaneIdx::Primary);
        assert_eq!(PaneIdx::from_hole(1, false), PaneIdx::Secondary);
        assert_eq!(
            PaneIdx::from_hole(7, false),
            PaneIdx::Primary,
            "never an error"
        );
        // Mirrored (a `side = "left"` split): the secondary takes the FIRST hole.
        assert_eq!(PaneIdx::Primary.hole(true), 1);
        assert_eq!(PaneIdx::Secondary.hole(true), 0);
        assert_eq!(PaneIdx::from_hole(0, true), PaneIdx::Secondary);
        assert_eq!(PaneIdx::from_hole(1, true), PaneIdx::Primary);
        assert_eq!(
            PaneIdx::from_hole(7, true),
            PaneIdx::Primary,
            "never an error"
        );
        for which in [PaneIdx::Primary, PaneIdx::Secondary] {
            for mirrored in [false, true] {
                assert_eq!(PaneIdx::from_hole(which.hole(mirrored), mirrored), which);
            }
        }
    }

    #[test]
    fn split_side_reports_the_config_split_only() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec_with_split("a", None), false).unwrap(); // side = Left
        r.add(&spec("b", "/tmp/b"), false).unwrap();
        r.split("b"); // a runtime ⌘D split has no side — it is never mirrored
        assert_eq!(r.split_side("a"), Some(warden_config::SplitSide::Left));
        assert_eq!(r.split_side("b"), None);
        assert_eq!(r.split_side("nope"), None);
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
        assert_eq!(r.clear_detached("t0"), DetachedEnded::Cold(None));
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
        assert_eq!(r.clear_detached("t0"), DetachedEnded::Cold(None));
        assert!(!r.is_detached("t0"));
        assert!(r.is_split("t0"));
        assert_eq!(
            r.clear_detached("t0"),
            DetachedEnded::Cold(None),
            "nothing detached any more → no-op"
        );
        assert_eq!(r.clear_detached("nope"), DetachedEnded::Cold(None));
    }

    #[test]
    fn locate_surface_is_none_for_an_unknown_id() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("a", "/tmp/a"), false).unwrap();
        assert_eq!(r.locate_surface(999_999), None);
    }

    fn rex_spec(id: &str, server: &str) -> TabSpec {
        let mut s = spec(id, "/tmp");
        s.server = Some(server.into());
        s.startup = Some("amux".into());
        s
    }

    /// A registry whose Rex opener records each request instead of opening anything.
    fn rex_registry() -> (Registry, std::sync::Arc<std::sync::Mutex<Vec<RexOpen>>>) {
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let log = asked.clone();
        r.set_rex_opener(Box::new(move |req| {
            log.lock().unwrap().push(req);
            Ok(())
        }));
        (r, asked)
    }

    #[test]
    fn probe_targets_excludes_server_tabs() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec_with_probe("plain", "/tmp", Some("p")), false)
            .unwrap();
        let mut rex = spec_with_probe("rex", "/tmp", Some("p"));
        rex.server = Some("local".into());
        r.add(&rex, false).unwrap();
        let ids: Vec<String> = r.probe_targets().into_iter().map(|t| t.0).collect();
        assert_eq!(ids, ["plain"]);
        assert!(
            !r.tab_has_probe("rex"),
            "no probe runs, so no probe burst awaits it"
        );
    }

    #[test]
    fn end_target_destroy_on_server_tab_is_rex_destroy() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut rex = rex_spec("rex", "local");
        rex.kill = Some("k".into());
        rex.suspend = Some("s".into());
        r.add(&rex, false).unwrap();
        let mut plain = spec("plain", "/tmp");
        plain.kill = Some("k".into());
        r.add(&plain, false).unwrap();

        assert_eq!(
            r.end_target("rex", EndSession::Destroy),
            Some(EndTarget::Rex(vec!["rex".into(), "rex::2".into()]))
        );
        assert_eq!(
            r.end_target("rex", EndSession::Suspend),
            Some(EndTarget::Command("/tmp".into(), "rex".into(), "s".into())),
            "a local Rex tab keeps its suspend command"
        );
        assert!(r.tab_dtos()[0].has_suspend);
        assert_eq!(
            r.end_target("plain", EndSession::Destroy),
            Some(EndTarget::Command(
                "/tmp".into(),
                "plain".into(),
                "k".into()
            ))
        );
    }

    #[test]
    fn remote_rex_tab_offers_no_suspend() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut remote = rex_spec("remote", "remote-host");
        remote.suspend = Some("s".into());
        r.add(&remote, false).unwrap();
        assert!(!r.tab_dtos()[0].has_suspend);
        assert_eq!(r.end_target("remote", EndSession::Suspend), None);
    }

    #[test]
    fn a_server_tab_offers_destroy_and_a_presence_dot() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("rex", "local"), false).unwrap();
        let dto = &r.tab_dtos()[0];
        assert!(dto.has_kill, "☠ destroys its Rex sessions");
        assert!(dto.has_probe, "its dot comes from Rex");
    }

    #[test]
    fn a_server_tab_never_offers_start() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("rex", "local"), false).unwrap();
        assert!(
            !r.tab_dtos()[0].has_cmd,
            "opening a Rex tab is its start; typing cmd would land in the running program"
        );
        assert!(!r.start_session("rex"));
    }

    #[test]
    fn a_long_title_caps_the_session_label() {
        let (mut r, asked) = rex_registry();
        let mut s = rex_spec("t", "local");
        s.title = "é".repeat(200);
        r.add(&s, false).unwrap();
        let _ = r.activate("t");
        let asked = asked.lock().unwrap();
        let label = &asked[0].label;
        assert_eq!(label.chars().count(), crate::rex::SESSION_LABEL_MAX);
        assert!(label.starts_with('é'));
    }

    #[test]
    fn activating_a_server_tab_asks_for_its_session_and_stays_cold() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), false).unwrap();
        assert!(!r.activate("t").unwrap(), "nothing spawned yet");
        assert!(r.is_connecting("t"));
        assert!(!r.tab_dtos()[0].spawned);
        {
            let asked = asked.lock().unwrap();
            assert_eq!(asked.len(), 1);
            let req = &asked[0];
            assert_eq!(req.pane_key, "t");
            assert_eq!(req.server, "local");
            assert_eq!(req.dir, PathBuf::from("/tmp"));
            assert_eq!(req.startup.as_deref(), Some("amux"));
            assert_eq!(req.label, "t · main");
        }
        let _ = r.activate("t");
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "one open in flight per pane"
        );
    }

    #[test]
    fn an_eager_server_tab_asks_at_add() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), true).unwrap();
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert!(r.is_connecting("t"));
    }

    #[test]
    fn a_failed_open_warns_with_its_reason_and_retries_on_activate() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), false).unwrap();
        let _ = r.activate("t");
        let req = asked.lock().unwrap()[0].clone();
        let landed = r.rex_opened(
            &req.pane_key,
            req.token,
            Err("Rex server unreachable: local".into()),
        );
        assert!(matches!(landed, RexLanded::Failed));
        assert!(!r.is_connecting("t"));
        assert_eq!(
            r.tab_dtos()[0].warn.as_deref(),
            Some("Rex server unreachable: local")
        );
        let _ = r.activate("t");
        assert_eq!(asked.lock().unwrap().len(), 2, "activating again retries");
    }

    #[test]
    fn an_open_answering_after_unload_is_ignored() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), false).unwrap();
        let _ = r.activate("t");
        let req = asked.lock().unwrap()[0].clone();
        r.unload("t");
        assert!(!r.is_connecting("t"));
        let target = crate::rex::PaneTarget {
            endpoint: rex_client::Endpoint::Unix("/x.sock".into()),
            session_id: "session:s".into(),
            block_id: "block:b".into(),
        };
        let landed = r.rex_opened(&req.pane_key, req.token, Ok(RexAnswer::Pane(target)));
        assert!(matches!(landed, RexLanded::Stale));
        assert!(!r.tab_dtos()[0].spawned);
    }

    fn unclaimed_spec(session: &str) -> TabSpec {
        let mut s = rex_spec(&format!("rex:{session}"), "local");
        s.dir = PathBuf::new();
        s.startup = None;
        s
    }

    #[test]
    fn an_unclaimed_session_tab_has_no_dir_to_miss() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&unclaimed_spec("session:x"), false).unwrap();
        assert_eq!(r.tab_dtos()[0].warn, None);
    }

    #[test]
    fn an_unclaimed_session_tab_refuses_a_split() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&unclaimed_spec("session:x"), false).unwrap();
        r.add(&rex_spec("t", "local"), false).unwrap();
        assert!(
            !r.split("rex:session:x"),
            "its session is shown, never one made for a split"
        );
        assert!(!r.is_split("rex:session:x"));
        assert!(r.split("t"));
        assert!(r.split("t"), "already split");
    }

    #[test]
    fn a_remote_tabs_dir_is_not_checked_here() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut remote = rex_spec("remote", "remote-host");
        remote.dir = PathBuf::from("/no/such/dir/xyz");
        r.add(&remote, false).unwrap();
        let mut local = rex_spec("local", "local");
        local.dir = PathBuf::from("/no/such/dir/xyz");
        r.add(&local, false).unwrap();
        let dtos = r.tab_dtos();
        assert_eq!(dtos[0].warn, None, "a remote dir lives on its server");
        assert!(dtos[1].warn.is_some(), "a local server's dir is this Mac's");
    }

    #[test]
    fn rex_pane_keys_are_the_pane_spec_ids() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.add(&spec("plain", "/tmp"), false).unwrap();
        r.split("t");
        assert_eq!(r.rex_pane_key("t", PaneIdx::Primary).as_deref(), Some("t"));
        assert_eq!(
            r.rex_pane_key("t", PaneIdx::Secondary).as_deref(),
            Some("t::2")
        );
        assert_eq!(r.rex_pane_key("plain", PaneIdx::Primary), None);
        assert_eq!(r.rex_tabs(), [("t".to_string(), "local".to_string())]);
    }

    // --- agent tabs (a Rex tab's main pane) ----------------------------------
    //
    // A landed agent answer with live agents spawns a relay surface, which needs AppKit; these
    // tests reach agent slots through `force_agents` (no target, so nothing can spawn) or an
    // answer with nothing live, and read which agent a pane shows from `visible_agent`, the
    // decision `activate` and `select_agent` show and hide surfaces by.

    fn nothing_live(r: &mut Registry, req: &RexOpen) -> RexLanded {
        r.rex_opened(
            &req.pane_key,
            req.token,
            Ok(RexAnswer::Agents(AgentsOpened::default())),
        )
    }

    #[test]
    fn selecting_agent_hides_others() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.add(&rex_spec("u", "local"), false).unwrap();
        r.force_agents("t", &["r1", "r2", "r3"], Some("r1"));
        let _ = r.activate("t");
        assert_eq!(r.visible_agent("t"), Some("r1"));
        assert!(r.select_agent("t", "r2").unwrap());
        assert_eq!(
            r.visible_agent("t"),
            Some("r2"),
            "only the selected one shows"
        );
        assert!(
            !r.select_agent("t", "gone").unwrap(),
            "an unknown agent changes nothing"
        );
        assert!(!r.select_agent("nope", "r1").unwrap());
        assert_eq!(r.visible_agent("t"), Some("r2"));
        assert_eq!(
            r.agent_keys("t"),
            ["t::agent:r1", "t::agent:r2", "t::agent:r3"]
        );
        // Another tab on screen hides every agent of this one; selection survives.
        r.force_agents("u", &["x"], Some("x"));
        let _ = r.activate("u");
        assert_eq!(r.visible_agent("t"), None);
        assert!(r.select_agent("t", "r3").unwrap());
        assert_eq!(
            r.visible_agent("t"),
            None,
            "selecting off screen shows nothing"
        );
        let _ = r.activate("t");
        assert_eq!(r.visible_agent("t"), Some("r3"));
    }

    #[test]
    fn locate_surface_finds_agent_key() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.split("t");
        r.force_agents("t", &["r1"], Some("r1"));
        let at = |key: &str| r.locate_key(key);
        assert_eq!(
            at(&agent_key("t", "r1")),
            Some(("t".to_string(), PaneIdx::Primary, Some("r1".to_string())))
        );
        assert_eq!(
            at("t::2"),
            Some(("t".to_string(), PaneIdx::Secondary, None))
        );
        assert_eq!(at("t"), Some(("t".to_string(), PaneIdx::Primary, None)));
        assert_eq!(at(&agent_key("t", "zz")), None);
        assert_eq!(agent_key("t", "r1"), "t::agent:r1");
    }

    #[test]
    fn auto_open_after_restart_wakes_nothing() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), true).unwrap();
        let req = asked.lock().unwrap()[0].clone();
        assert!(
            req.auto && req.agents,
            "an eager add is an automatic agent-pane open"
        );
        // Every record came back asleep: the hub found nothing live.
        assert!(matches!(
            nothing_live(&mut r, &req),
            RexLanded::Opened(Ok(()))
        ));
        // build_window's activate and a hot reload's spawn don't ask again.
        let _ = r.activate("t");
        let _ = r.ensure_spawned_by_id("t");
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(asked.iter().all(|o| o.auto), "no open woke anything");
        assert!(!r.is_spawned("t"), "no surface");
        assert!(!r.is_connecting("t"));
        assert!(r.agent_keys("t").is_empty());
    }

    #[test]
    fn user_open_wakes_last_selected_only() {
        // The registry's half: only a user open asks the hub to wake, once, with the tab's
        // `resume` as configured now. Which record wakes is the hub's (`rex::agent_open`).
        let (mut r, asked) = rex_registry();
        let mut s = rex_spec("t", "local");
        s.resume = Some("old-wrapper".into());
        r.add(&s, true).unwrap();
        let req = asked.lock().unwrap()[0].clone();
        nothing_live(&mut r, &req);
        r.set_meta(
            "t",
            &warden_config::TabMeta {
                title: "t".into(),
                group: None,
                probe: None,
                kill: None,
                suspend: None,
                resume: Some("agent-wrapper".into()),
                split: None,
            },
            false,
            Vec::new(),
        );
        assert!(
            !r.activate_user("t").unwrap(),
            "nothing on screen until the answer"
        );
        assert!(r.is_connecting("t"));
        let _ = r.activate_user("t");
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "one wake in flight");
        assert!(!asked[1].auto);
        assert_eq!(asked[1].resume.as_deref(), Some("agent-wrapper"));
    }

    #[test]
    fn a_user_open_during_an_auto_open_wakes_when_it_lands_asleep() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), true).unwrap();
        let first = asked.lock().unwrap()[0].clone();
        assert!(first.auto);
        // The user clicks while the automatic open is still in flight.
        let _ = r.activate_user("t");
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "one open in flight per pane"
        );
        nothing_live(&mut r, &first);
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "the click's wake follows the asleep answer");
        assert!(!asked[1].auto);
        assert!(r.is_connecting("t"));
    }

    #[test]
    fn an_auto_open_landing_asleep_with_no_click_stays_asleep() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), true).unwrap();
        let first = asked.lock().unwrap()[0].clone();
        let _ = r.activate("t");
        nothing_live(&mut r, &first);
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert!(!r.is_connecting("t"));
    }

    #[test]
    fn an_exited_agent_leans_to_a_running_one_not_yet_shown() {
        // Only the selected agent has a surface; the others are running in Rex with their
        // terminal in hand. The shown one exiting must not take the pane cold.
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_ready_agents("t", &["r1", "r2", "r3"], Some("r2"));
        assert_eq!(r.close_agent("t", "r2"), Some(AgentClosed::PaneLive));
        assert_eq!(r.agent_keys("t"), ["t::agent:r1", "t::agent:r3"]);
        assert_eq!(r.selected_agent("t"), Some("r1"), "leans up the list");
        // With nothing else running, the pane goes cold.
        r.force_agents("t", &["r1", "r2"], Some("r1"));
        assert_eq!(r.close_agent("t", "r1"), Some(AgentClosed::PaneCold(None)));
        assert!(r.agent_keys("t").is_empty());
        assert_eq!(r.close_agent("t", "gone"), None);
    }

    #[test]
    fn a_popped_out_agent_keeps_its_agent_key() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.add(&spec("p", "/tmp"), false).unwrap();
        r.force_agents("t", &["r1", "r2"], Some("r2"));
        // What pop-out records as the travelling agent, and the key its exit and size then name.
        let agent = r.selected_agent("t");
        assert_eq!(agent, Some("r2"));
        let t = r.spec_of("t").unwrap();
        assert_eq!(
            shown_rex_key(t, PaneIdx::Primary, agent).as_deref(),
            Some("t::agent:r2")
        );
        assert_eq!(
            shown_rex_key(t, PaneIdx::Secondary, agent).as_deref(),
            Some("t::2"),
            "a split pane is never an agent"
        );
        assert_eq!(
            shown_rex_key(t, PaneIdx::Primary, None).as_deref(),
            Some("t")
        );
        assert_eq!(r.selected_agent("p"), None);
        assert_eq!(
            shown_rex_key(r.spec_of("p").unwrap(), PaneIdx::Primary, None),
            None
        );
    }

    fn target(block: &str) -> PaneTarget {
        PaneTarget {
            endpoint: rex_client::Endpoint::Unix("/x.sock".into()),
            session_id: "session:s".into(),
            block_id: block.into(),
        }
    }

    #[test]
    fn sync_follows_an_ops_answer_off_screen() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_ready_agents("t", &["r1", "r2"], Some("r1"));
        let found = AgentsOpened {
            agents: vec![
                ("r2".into(), target("block:r2")),
                ("r3".into(), target("block:r3")),
            ],
            selected: Some("r3".into()),
        };
        r.sync_agents("t", found).unwrap();
        assert_eq!(r.agent_keys("t"), ["t::agent:r2", "t::agent:r3"]);
        assert_eq!(r.selected_agent("t"), Some("r3"));
    }

    #[test]
    fn sync_leaves_an_unopened_pane_to_its_open() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        let found = AgentsOpened {
            agents: vec![("r1".into(), target("block:r1"))],
            selected: Some("r1".into()),
        };
        r.sync_agents("t", found).unwrap();
        assert!(r.agent_keys("t").is_empty());
    }

    #[test]
    fn sync_with_nothing_live_leaves_the_pane_asleep() {
        let (mut r, asked) = rex_registry();
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_ready_agents("t", &["r1"], Some("r1"));
        r.sync_agents("t", AgentsOpened::default()).unwrap();
        assert!(r.agent_keys("t").is_empty());
        let _ = r.activate("t");
        assert!(
            asked.lock().unwrap().is_empty(),
            "warden's own open wakes nothing"
        );
        let _ = r.activate_user("t");
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(!asked[0].auto, "the user's click wakes the last-selected");
    }

    #[test]
    fn agent_request_names_the_main_pane() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        let mut s = rex_spec("t", "local");
        s.resume = Some("wrapper".into());
        r.add(&s, false).unwrap();
        r.add(&spec("p", "/tmp"), false).unwrap();
        r.add(&unclaimed_spec("session:x"), false).unwrap();
        let req = r.agent_request("t").expect("an agent pane");
        assert_eq!(
            (
                req.pane_key.as_str(),
                req.server.as_str(),
                req.label.as_str()
            ),
            ("t", "local", "t · main")
        );
        assert_eq!(req.startup.as_deref(), Some("amux"));
        assert_eq!(req.resume.as_deref(), Some("wrapper"));
        assert!(req.agents && !req.auto);
        assert_eq!(r.agent_request("p"), None);
        assert_eq!(r.agent_request("rex:session:x"), None);
    }

    #[test]
    fn the_agent_list_and_its_keys_follow_the_main_pane() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&spec("p", "/tmp"), false).unwrap();
        r.add(&unclaimed_spec("session:x"), false).unwrap();
        r.add(&rex_spec("t", "local"), false).unwrap();
        let agents: Vec<_> = r.tab_dtos().into_iter().map(|d| (d.id, d.agents)).collect();
        assert_eq!(
            agents,
            vec![
                ("p".to_string(), false),
                ("rex:session:x".to_string(), false),
                ("t".to_string(), true)
            ]
        );
        r.active = Some("p".into());
        assert_eq!(r.active_agent_tab(), None);
        r.active = Some("t".into());
        assert_eq!(r.active_agent_tab(), Some("t"));
    }

    #[test]
    fn a_popped_out_agent_ending_leans_its_pane_to_a_sibling() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_ready_agents("t", &["r1", "r2", "r3"], Some("r2"));
        r.force_detached_agent("t", "r2");
        r.add(&spec("p", "/tmp"), false).unwrap();
        // The origin shows the tab (its pane is out, so nothing spawns here).
        let _ = r.activate("t");
        assert_eq!(r.clear_detached("t"), DetachedEnded::SiblingShown);
        assert_eq!(r.active_tab(), Some("t"), "the tab stays on screen");
        assert_eq!(r.agent_keys("t"), ["t::agent:r1", "t::agent:r3"]);
        assert_eq!(r.selected_agent("t"), Some("r1"), "not a removed record");
        assert!(!r.is_detached("t"));
    }

    #[test]
    fn a_selection_made_in_the_origin_survives_the_popped_out_agent_ending() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_ready_agents("t", &["r1", "r2", "r3"], Some("r2"));
        r.force_detached_agent("t", "r2");
        r.select_agent("t", "r3").unwrap();
        assert_eq!(r.clear_detached("t"), DetachedEnded::SiblingShown);
        assert_eq!(r.selected_agent("t"), Some("r3"));
    }

    #[test]
    fn a_popped_out_last_agent_ending_takes_the_tab_cold() {
        let mut r = Registry::new(std::ptr::null_mut(), rect());
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_ready_agents("t", &["r1"], Some("r1"));
        r.force_detached_agent("t", "r1");
        let _ = r.activate("t");
        assert_eq!(r.clear_detached("t"), DetachedEnded::Cold(None));
        assert_eq!(r.active_tab(), None);
        assert!(r.agent_keys("t").is_empty());
    }

    #[test]
    fn an_unloaded_agent_pane_drops_its_agents() {
        let (mut r, _) = rex_registry();
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.force_agents("t", &["r1", "r2"], Some("r2"));
        r.unload("t");
        assert!(
            r.agent_keys("t").is_empty(),
            "the next open reconciles afresh"
        );
    }

    #[test]
    fn plain_tab_unchanged() {
        let (mut r, asked) = rex_registry();
        r.add(&spec("p", "/tmp"), false).unwrap();
        r.split("p");
        r.add(&unclaimed_spec("session:x"), false).unwrap();
        assert!(r.agent_keys("p").is_empty());
        assert!(!r.select_agent("p", "r1").unwrap());
        assert_eq!(r.visible_agent("p"), None);
        assert_eq!(
            r.locate_key("p"),
            Some(("p".to_string(), PaneIdx::Primary, None))
        );
        assert_eq!(
            r.locate_key("p::2"),
            Some(("p".to_string(), PaneIdx::Secondary, None))
        );
        // An unclaimed session's row shows the one session it lists, not agent tabs.
        let _ = r.activate_user("rex:session:x");
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(!asked[0].agents);
        // A Rex tab's split pane keeps its one-session key.
        drop(asked);
        r.add(&rex_spec("t", "local"), false).unwrap();
        r.split("t");
        r.force_agents("t", &["r1"], Some("r1"));
        assert_eq!(
            r.rex_pane_key("t", PaneIdx::Secondary).as_deref(),
            Some("t::2")
        );
        assert_eq!(r.agent_keys("t"), ["t::agent:r1"]);
    }
}
