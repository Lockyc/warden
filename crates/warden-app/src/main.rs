mod ffi;
mod geometry;
mod plan;
mod scanner;
mod surface;

#[cfg(not(target_os = "macos"))]
compile_error!(
    "warden-app currently targets macOS only (libghostty surface embed). Linux is a later spike."
);

#[cfg(target_os = "macos")]
mod logging;

#[cfg(target_os = "macos")]
mod manager;

#[cfg(target_os = "macos")]
mod notify;

#[cfg(target_os = "macos")]
mod pane_menu;

#[cfg(target_os = "macos")]
mod probe;
mod project_colour;

#[cfg(target_os = "macos")]
mod registry;

#[cfg(target_os = "macos")]
mod session;
mod split_write;

#[cfg(target_os = "macos")]
mod rex;

#[cfg(target_os = "macos")]
mod windows;

#[cfg(target_os = "macos")]
use manager::{InitDto, WindowManager};
#[cfg(target_os = "macos")]
use shell_core::windows::{open_request, WindowSet};

use geometry::WebRect;

// Menu-item IDs, matched in the Builder's on_menu_event handler. The App/Config/Window submenus,
// Close Tab / Close Window / Pop Out Tab, and the tab-nav block are all the shared spine now
// (`shell_core::menu`); warden's own items keep local ids.
// Reopen Last Closed (⌘⇧T) is warden-only — curator and lector have no equivalent, so this stays
// a local item spliced into the spine's Window submenu rather than added to the spine itself
// (YAGNI — add it there only if a sibling app wants it too).
const MENU_WINDOW_REOPEN_LAST: &str = "window_reopen_last";

/// Tab ▸ Split (⌘D) — warden-only, spliced into warden's OWN Tab submenu composition rather than
/// shell-core's shared spine: a split is terminal-specific, and curator/lector have no equivalent
/// (same YAGNI reasoning as `MENU_WINDOW_REOPEN_LAST`). The id doubles as the event name emitted
/// to the focused window — the chrome's own `activeId` decides which tab to split, so the payload
/// carries only the window label, matching Close Tab / Pop Out Tab below.
const MENU_TAB_SPLIT: &str = "warden:split-pane";

/// Tab ▸ Suspend Session (⌘⇧S) / Terminate Session (⌘⇧K) — the keyboard route to the sidebar
/// confirm row's ⏻ and ☠, warden-only for the same reason as `MENU_TAB_SPLIT` (a terminal session
/// is what they end). The id doubles as the event name, label-only payload; the chrome resolves
/// the active tab and hands it to chrome-core's `requestEnd`, which suspends at once and arms the
/// confirm row before a terminate.
const MENU_TAB_SUSPEND: &str = "warden:suspend-tab";
const MENU_TAB_TERMINATE: &str = "warden:terminate-tab";

/// Tab ▸ New Agent Tab (⌘T) / Sleep Agent Tab (⌥⌘S) — act on the agent tab list of the focused
/// window's active tab, enabled only while there is one ([`sync_agent_menu`]). The id doubles as
/// the event name, sent with the tab's id — from either of its panes, as ⌘W's agent close; a
/// scratch tab is ⌃⌘T's (`MENU_SCRATCH_NEW`). ⌘⇧T reaches the same list as
/// `warden:agent-reopen` when it has a closed agent tab (see `MENU_WINDOW_REOPEN_LAST`'s handler)
/// — from a scratch pane too: a closed scratch tab is gone from Rex, so there is nothing of the
/// column's to reopen.
const MENU_AGENT_NEW: &str = "warden:agent-new";
const MENU_AGENT_SLEEP: &str = "warden:agent-sleep";
const EVENT_AGENT_REOPEN: &str = "warden:agent-reopen";

/// Tab ▸ Previous / Next Agent Tab (⌘↑ / ⌘↓), Move Agent Tab Up / Down (⌥⇧⌘↑ / ⌥⇧⌘↓) and Fork
/// Agent Tab (⌥⌘T): the selected agent tab of the focused window's active tab, from any of its
/// panes, enabled only while the step can act ([`sync_agent_menu`]) — a disabled item leaves its
/// chord to the terminal.
const MENU_AGENT_PREV: &str = "warden:agent-prev";
const MENU_AGENT_NEXT: &str = "warden:agent-next";
const MENU_AGENT_MOVE_UP: &str = "warden:agent-move-up";
const MENU_AGENT_MOVE_DOWN: &str = "warden:agent-move-down";
const MENU_AGENT_FORK: &str = "warden:agent-fork";

/// ⌘K / ⌘J: Previous / Next Agent Tab's second chords, each its own menu item (a menu item carries
/// one key equivalent) kept off screen by [`hide_alias_items`], which finds them by these titles.
/// Same enablement and action as the visible pair.
const MENU_AGENT_PREV_ALIAS: &str = "warden:agent-prev-alias";
const MENU_AGENT_NEXT_ALIAS: &str = "warden:agent-next-alias";
const ALIAS_TITLES: [&str; 2] = ["Previous Agent Tab (⌘K)", "Next Agent Tab (⌘J)"];

/// ⌘W in a Rex tab with a selected agent tab, from either of its panes: the spine's Close Tab,
/// retitled Close Agent Tab by [`sync_agent_menu`], closes that agent instead of unloading the
/// project tab. The event the chrome runs the row's own Close from, confirm included.
const EVENT_AGENT_CLOSE: &str = "warden:agent-close";

/// Tab ▸ Close Project Tab (⌥⌘W): unloads the active tab whatever ⌘W closes in the focused pane,
/// so the one chord always works — in a pop-out too, which then closes with nothing left running.
const MENU_TAB_CLOSE_PROJECT: &str = "warden:close-project-tab";

/// Tab ▸ Focus Left / Right Pane (⌥⌘← / ⌥⌘→): the active tab's pane on that side of the screen,
/// enabled while there is one ([`registry::Registry::pane_to`]).
const MENU_PANE_LEFT: &str = "warden:pane-left";
const MENU_PANE_RIGHT: &str = "warden:pane-right";

/// Tab ▸ New Scratch Tab (⌃⌘T): a new scratch tab in the focused window's active tab's Rex
/// scratch column ([`scratch_run`]), enabled only while it has one.
const MENU_SCRATCH_NEW: &str = "warden:scratch-new";

/// Titles [`sync_agent_menu`] retitles from, appending a reason or naming what the item does in
/// the focused pane.
const SPLIT_TITLE: &str = "Split";
const AGENT_NEW_TITLE: &str = "New Agent Tab";
const SCRATCH_NEW_TITLE: &str = "New Scratch Tab";
const SUSPEND_TITLE: &str = "Suspend Session";
const TERMINATE_TITLE: &str = "Terminate Session…";

/// Why Suspend / Terminate Session can't act in the focused window, if they can't: both run
/// through the sidebar row's confirm, which a pop-out has none of.
const END_SESSION_POPOUT: &str = "dock the tab to end its session";

/// Suspend or Terminate Session's enabled state and title: `title` as is, or disabled with
/// `why` beside it.
fn end_session_item(title: &str, why: Option<&str>) -> (bool, String) {
    match why {
        Some(why) => (false, format!("{title} ({why})")),
        None => (true, title.to_owned()),
    }
}

/// The items whose enabled state follows the focused window's active tab, kept from the last menu
/// build so [`sync_agent_menu`] can set it without a rebuild.
#[cfg(target_os = "macos")]
#[derive(Clone)]
struct AgentItems {
    /// Split (⌘D), titled for what it does in the focused pane ([`focused_split`]).
    split: tauri::menu::MenuItem<tauri::Wry>,
    new: tauri::menu::MenuItem<tauri::Wry>,
    sleep: tauri::menu::MenuItem<tauri::Wry>,
    reopen: tauri::menu::MenuItem<tauri::Wry>,
    /// The visible item, then its hidden alias.
    prev: [tauri::menu::MenuItem<tauri::Wry>; 2],
    next: [tauri::menu::MenuItem<tauri::Wry>; 2],
    move_up: tauri::menu::MenuItem<tauri::Wry>,
    move_down: tauri::menu::MenuItem<tauri::Wry>,
    /// The spine's Close Tab (⌘W), retitled Close Agent Tab while it closes one
    /// ([`agent_closable`]).
    close_tab: tauri::menu::MenuItem<tauri::Wry>,
    fork: tauri::menu::MenuItem<tauri::Wry>,
    pane_left: tauri::menu::MenuItem<tauri::Wry>,
    pane_right: tauri::menu::MenuItem<tauri::Wry>,
    scratch_new: tauri::menu::MenuItem<tauri::Wry>,
    /// Suspend / Terminate Session (⌘⇧S / ⌘⇧K), disabled with the reason in a pop-out
    /// ([`END_SESSION_POPOUT`]).
    suspend: tauri::menu::MenuItem<tauri::Wry>,
    terminate: tauri::menu::MenuItem<tauri::Wry>,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct AgentMenu(std::sync::Mutex<Option<AgentItems>>);

/// Tab ▸ Sleep Server ▸ `<server>` — one item per Rex server; the id is this prefix plus the
/// server.
const MENU_SLEEP_SERVER: &str = "warden:sleep-server:";

/// The Sleep Server items by server, kept from the last menu build so their enabled state and
/// reason follow the hub's connections without a rebuild ([`sync_server_menu`]).
#[cfg(target_os = "macos")]
#[derive(Default)]
struct ServerMenu(std::sync::Mutex<Vec<(String, tauri::menu::MenuItem<tauri::Wry>)>>);

/// Every Rex server warden knows: named by a tab in an open window, the store, or the hub — sorted.
#[cfg(target_os = "macos")]
fn rex_servers(app: &tauri::AppHandle) -> Vec<String> {
    use tauri::Manager;
    let mut servers: Vec<String> = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows
            .values()
            .flat_map(|ws| ws.registry.rex_tabs().into_iter().map(|(_, s)| s))
            .collect()
    };
    if let Some(hub) = app.try_state::<rex::RexHub>() {
        servers.extend(hub.servers());
        servers.extend(hub.store_servers());
    }
    servers.sort();
    servers.dedup();
    servers
}

/// Enable each Sleep Server item while its server can sleep, else disable it with the reason
/// beside its name ([`rex::RexHub::stoppable`]). Main thread: off it `set_enabled` waits on it.
#[cfg(target_os = "macos")]
pub(crate) fn sync_server_menu(app: &tauri::AppHandle) {
    use tauri::Manager;
    let (Some(menu), Some(hub)) = (
        app.try_state::<ServerMenu>(),
        app.try_state::<rex::RexHub>(),
    ) else {
        return;
    };
    // The home surface has no chrome to confirm a loss in or show an error.
    let homeless = focused_label(app).as_deref() == Some(shell_core::home::HOME_LABEL);
    let items = menu.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for (server, item) in items {
        let why = match homeless {
            true => Some("open a window first"),
            false => hub.stoppable(&server).err(),
        };
        let _ = item.set_enabled(why.is_none());
        let _ = item.set_text(match why {
            None => server.clone(),
            Some(why) => format!("{server} — {why}"),
        });
    }
}

/// The focused window's label.
#[cfg(target_os = "macos")]
fn focused_label(app: &tauri::AppHandle) -> Option<String> {
    use tauri::Manager;
    let w = app
        .webview_windows()
        .into_values()
        .find(|w| w.is_focused().unwrap_or(false))?;
    Some(w.label().to_string())
}

/// `(label, tab)` when the focused window's active tab has a Rex scratch column whose open has
/// landed.
#[cfg(target_os = "macos")]
fn focused_scratch_column(app: &tauri::AppHandle) -> Option<(String, String)> {
    use tauri::Manager;
    let label = focused_label(app)?;
    let st = app.state::<ManagerState>();
    let m = st.lock();
    let reg = &m.windows.get(&label)?.registry;
    let tab = reg.active_tab()?.to_owned();
    reg.rex_column(&tab)?;
    Some((label, tab))
}

/// What the focused window's active tab offers the agent menu items: `(label, tab, sleep,
/// reopen)` when it holds agent tabs — its selected agent tab can sleep, it has a closed one to
/// reopen — else `None`.
#[cfg(target_os = "macos")]
fn focused_agent_tab(app: &tauri::AppHandle) -> Option<(String, String, bool, bool)> {
    use tauri::Manager;
    let label = focused_label(app)?;
    let tab = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows
            .get(&label)?
            .registry
            .active_agent_tab()?
            .to_owned()
    };
    let (sleep, reopen) = rex::lifecycle::menu_actions(app, &tab);
    Some((label, tab, sleep, reopen))
}

/// What the focused window's active tab offers the agent-step items: `(label, tab, agent list
/// when it holds agent tabs)`. The agent list answers from any of the tab's panes, so the agent
/// keys beat libghostty's defaults for the same chords (⌘↑ / ⌘↓ / ⌘K / ⌘J) in a scratch shell too.
#[cfg(target_os = "macos")]
fn focused_tab_keys(
    app: &tauri::AppHandle,
) -> Option<(String, String, Option<rex::lifecycle::AgentListDto>)> {
    use tauri::Manager;
    let label = focused_label(app)?;
    let (tab, agents) = {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        let reg = &m.windows.get(&label)?.registry;
        let tab = reg.active_tab()?.to_owned();
        let agents = reg.active_agent_tab() == Some(tab.as_str());
        (tab, agents)
    };
    let list = agents
        .then(|| rex::lifecycle::current_list(app, &tab))
        .flatten();
    Some((label, tab, list))
}

/// The pane to `side` of the focused pane in the focused window's active tab, and, with `focus`,
/// give it keyboard focus. Whether there is one.
#[cfg(target_os = "macos")]
fn focused_pane_to(app: &tauri::AppHandle, side: registry::Side, focus: bool) -> bool {
    use tauri::Manager;
    let Some(label) = focused_label(app) else {
        return false;
    };
    let st = app.state::<ManagerState>();
    let m = st.lock();
    let Some(reg) = m.windows.get(&label).map(|ws| &ws.registry) else {
        return false;
    };
    let Some(tab) = reg.active_tab() else {
        return false;
    };
    match reg.pane_to(tab, side) {
        Some(pane) => !focus || reg.focus_pane_surface(tab, pane),
        None => false,
    }
}

/// What the menu bar's Split (⌘D) does in the focused window: `None` with no tab to split there;
/// `Err` is why it can't, shown on the disabled item.
#[cfg(target_os = "macos")]
fn focused_split(app: &tauri::AppHandle) -> Option<Result<pane_menu::SplitKind, String>> {
    use tauri::Manager;
    let label = focused_label(app)?;
    let st = app.state::<ManagerState>();
    let m = st.lock();
    let reg = &m.windows.get(&label)?.registry;
    Some(split_kind(reg, reg.active_tab()?))
}

/// Enable the agent tab menu items for the focused window's active tab: New Agent Tab while it
/// holds agent tabs, Sleep Agent Tab while its selected one can sleep, Reopen Last Closed while
/// there is an agent tab or a window to reopen, the agent-step items (Fork Agent Tab among them)
/// while the step can act, Focus Left / Right Pane while there's a pane that way, and New Scratch
/// Tab while it has a Rex scratch column, Suspend / Terminate Session outside a pop-out; ⌘W is
/// titled for what it closes. A pop-out's one tab answers like any active tab. Runs on every menu
/// build, window focus, pane focus, tab activation, agent list change and scratch column change
/// (a Rex view landing); locks `ManagerState`, so callers must not hold it.
#[cfg(target_os = "macos")]
pub(crate) fn sync_agent_menu(app: &tauri::AppHandle) {
    use rex::lifecycle::{agent_move, agent_step, Step};
    use tauri::Manager;
    let Some(menu) = app.try_state::<AgentMenu>() else {
        return;
    };
    let agent = focused_agent_tab(app);
    let split = focused_split(app);
    let keys = focused_tab_keys(app);
    let list = keys.as_ref().and_then(|k| k.2.as_ref());
    let pane_left = focused_pane_to(app, registry::Side::Left, false);
    let pane_right = focused_pane_to(app, registry::Side::Right, false);
    let reopen_window = reopen_target(app).is_some();
    let scratch = focused_scratch_column(app).is_some();
    let scratch_blocked = focused_label(app).and_then(|label| {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        let reg = &m.windows.get(&label)?.registry;
        reg.column_change_blocked(reg.active_tab()?)
    });
    let popped = focused_label(app).is_some_and(|label| {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        m.windows.get(&label).is_some_and(|ws| ws.origin.is_some())
    });
    let end_why = popped.then_some(END_SESSION_POPOUT);
    // Cloned out first: off the main thread `set_enabled` waits on it, and the main thread may be
    // waiting on this lock.
    let items = menu.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(i) = items {
        let _ = i.split.set_enabled(!matches!(split, Some(Err(_))));
        let _ = i.split.set_text(match &split {
            Some(Ok(kind)) => pane_menu::split_title(*kind).to_owned(),
            Some(Err(why)) => format!("{SPLIT_TITLE} ({why})"),
            None => SPLIT_TITLE.to_owned(),
        });
        let _ = i.new.set_enabled(agent.is_some());
        let _ = i.sleep.set_enabled(agent.as_ref().is_some_and(|a| a.2));
        let _ = i
            .reopen
            .set_enabled(agent.as_ref().is_some_and(|a| a.3) || reopen_window);
        let prev = list.is_some_and(|l| agent_step(l, Step::Prev).is_some());
        let next = list.is_some_and(|l| agent_step(l, Step::Next).is_some());
        for item in &i.prev {
            let _ = item.set_enabled(prev);
        }
        for item in &i.next {
            let _ = item.set_enabled(next);
        }
        let _ = i
            .move_up
            .set_enabled(list.is_some_and(|l| agent_move(l, Step::Prev).is_some()));
        let _ = i
            .move_down
            .set_enabled(list.is_some_and(|l| agent_move(l, Step::Next).is_some()));
        let _ = i.close_tab.set_text(if list.is_some_and(agent_closable) {
            "Close Agent Tab"
        } else {
            "Close Tab"
        });
        let _ = i
            .fork
            .set_enabled(list.is_some_and(|l| l.rows.iter().any(|r| r.selected && r.can_fork)));
        let _ = i.pane_left.set_enabled(pane_left);
        let _ = i.pane_right.set_enabled(pane_right);
        let _ = i
            .scratch_new
            .set_enabled(scratch && scratch_blocked.is_none());
        let _ = i.scratch_new.set_text(match scratch_blocked {
            Some(why) => format!("{SCRATCH_NEW_TITLE} ({why})"),
            None => SCRATCH_NEW_TITLE.to_owned(),
        });
        for (item, title) in [(&i.suspend, SUSPEND_TITLE), (&i.terminate, TERMINATE_TITLE)] {
            let (enabled, text) = end_session_item(title, end_why);
            let _ = item.set_enabled(enabled);
            let _ = item.set_text(text);
        }
    }
}

/// Whether ⌘W closes an agent tab in this list — the focused window's active tab's
/// ([`focused_tab_keys`]), a pop-out's included, whichever of its panes has focus: one is
/// selected. Otherwise ⌘W takes the spine's route, unloading the tab (closing a pop-out, which
/// docks it); a scratch pane closes from its right-click menu, `exit`, or the strip's ×.
#[cfg(target_os = "macos")]
fn agent_closable(list: &rex::lifecycle::AgentListDto) -> bool {
    list.rows.iter().any(|r| r.selected)
}

/// Close scratch pane `pane` of tab `tab` in window `label` — a pop-out's included — landing as
/// its program ending does: a Rex block is Rex's to close ([`rex::spawn::close_block`]); a plain
/// tab's block closes here and the chrome collapses the split (`warden:pane-closed`).
#[cfg(target_os = "macos")]
fn close_pane(
    app: &tauri::AppHandle,
    label: &str,
    tab: &str,
    pane: registry::PaneIdx,
) -> Result<(), String> {
    use tauri::Manager;
    let registry::PaneIdx::Block(i) = pane else {
        return Err("only a scratch pane closes".into());
    };
    let st = app.state::<ManagerState>();
    let mut m = st.lock();
    let reg = &mut m.windows.get_mut(label).ok_or("window not found")?.registry;
    match reg.begin_close_block(tab, i) {
        None => Err(format!(
            "tab {tab:?} has no scratch pane {} to close",
            i + 1
        )),
        Some(registry::BlockClose::Rex(key)) => {
            drop(m);
            rex::spawn::close_block(app, label, key);
            sync_agent_menu(app);
            Ok(())
        }
        Some(registry::BlockClose::Plain) => {
            reg.close_block(tab, i);
            drop(m);
            manager::announce_pane_closed(app, label, tab);
            sync_agent_menu(app);
            Ok(())
        }
    }
}

/// Run the agent-step or pane item `id` on the focused window's active tab (main thread). A step
/// that can't act any more (the list changed since the menu was synced) does nothing.
#[cfg(target_os = "macos")]
fn agent_key(app: &tauri::AppHandle, id: &str) {
    use rex::lifecycle::{agent_move, agent_step, Step};
    use tauri::Emitter;
    if id == MENU_PANE_LEFT || id == MENU_PANE_RIGHT {
        let side = if id == MENU_PANE_LEFT {
            registry::Side::Left
        } else {
            registry::Side::Right
        };
        focused_pane_to(app, side, true);
        return;
    }
    let Some((label, tab, Some(list))) = focused_tab_keys(app) else {
        return;
    };
    let step = |up| if up { Step::Prev } else { Step::Next };
    let done = match id {
        MENU_AGENT_PREV | MENU_AGENT_NEXT | MENU_AGENT_PREV_ALIAS | MENU_AGENT_NEXT_ALIAS => {
            let up = id == MENU_AGENT_PREV || id == MENU_AGENT_PREV_ALIAS;
            match agent_step(&list, step(up)) {
                Some(to) => rex::lifecycle::select(app, &label, &tab, &to),
                None => Ok(()),
            }
        }
        MENU_AGENT_MOVE_UP | MENU_AGENT_MOVE_DOWN => {
            match agent_move(&list, step(id == MENU_AGENT_MOVE_UP)) {
                Some((record, index)) => rex::lifecycle::edit(app, &tab, |s| {
                    s.place(&tab, &record, rex::agents::Place::Sibling(index))
                }),
                None => Ok(()),
            }
        }
        MENU_AGENT_FORK => match list.rows.iter().find(|r| r.selected && r.can_fork) {
            Some(r) => rex::lifecycle::run(
                app,
                &label,
                &tab,
                rex::lifecycle::AgentOp::Fork(r.id.clone()),
            ),
            None => Ok(()),
        },
        // ⌘W: the chrome runs the row's own Close, so an agent with nothing to resume asks first
        // exactly as from its ⋯ menu.
        _ => {
            let _ = app.emit_to(
                label.as_str(),
                EVENT_AGENT_CLOSE,
                serde_json::json!({ "label": label, "tabId": tab }),
            );
            Ok(())
        }
    };
    if let Err(e) = done {
        rex::lifecycle::banner(app, &label, &e);
    }
}

#[derive(serde::Deserialize)]
struct RectArg {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// All live window windows, behind a `Mutex`. Each `WindowState` holds a
/// `Registry` with `GhosttySurface: Send` values; all access is on the
/// main/UI thread (Tauri commands run there). [seam: manager only]
#[cfg(target_os = "macos")]
struct ManagerState(std::sync::Mutex<WindowManager>);

#[cfg(target_os = "macos")]
impl ManagerState {
    /// Lock the manager, recovering from a poisoned mutex. Surface-spawn failures no
    /// longer panic — they degrade to a cold tab (see `registry.rs` / `build_window`).
    /// What remains is the rare near-fatal AppKit failure: a multi-step op
    /// (`apply`/`materialize`) can still panic partway (e.g. an `ns_window`/window
    /// build `.expect`) and leave partial state, but recovering the guard keeps every
    /// subsequent command and the watcher reconcile alive instead of cascading one
    /// panic into permanently-dead IPC — the lesser evil.
    fn lock(&self) -> std::sync::MutexGuard<'_, WindowManager> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Holds the config-file watcher for the app's lifetime. The watcher stops
/// firing the moment it is dropped, so it must live in managed state rather
/// than as a local in `setup`. [seam: manager only]
#[cfg(target_os = "macos")]
struct WatcherState(#[allow(dead_code)] warden_config::Watcher);

/// Build and install the app menu: shell-core's App, Edit, Tab, Config and Window submenus, with
/// warden's own items as the Tab submenu's groups (Split; the session items; the agent and server
/// items) and Reopen Last Closed (⌘⇧T) spliced into the Window submenu. `mode` picks the digit
/// chords inside `build_tab_menu`. Edit's Copy / Paste / Select All reach a focused terminal
/// through its view's `copy:` / `paste:` / `selectAll:` (`surface/ghostty.rs`).
///
/// `set_menu` replaces the app-global menu wholesale, so a hot-reload that flips the mode just
/// rebuilds (see the watcher).
#[cfg(target_os = "macos")]
fn build_app_menu(
    app: &tauri::AppHandle,
    mode: warden_config::TabDigitKeys,
    window_entries: Vec<shell_core::menu::WindowEntry>,
    servers: Vec<String>,
) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};

    // About box carries the build stamp (shell_core::build_stamp → BUILD_GIT_SHA/BUILD_DATE) so a
    // glance confirms the installed app matches a given commit.
    let spine = shell_core::menu::build_spine(
        app,
        shell_core::menu::SpineConfig {
            app_name: "warden",
            windows: &window_entries,
            toggle_sidebar: true,
        },
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_GIT_SHA"),
        env!("BUILD_DATE"),
    )?;

    // Split (⌘D) — warden-only (see MENU_TAB_SPLIT's doc comment).
    let split_pane_item = MenuItemBuilder::with_id(MENU_TAB_SPLIT, SPLIT_TITLE)
        .accelerator("CmdOrCtrl+D")
        .build(app)?;
    let suspend_item = MenuItemBuilder::with_id(MENU_TAB_SUSPEND, SUSPEND_TITLE)
        .accelerator("Shift+Cmd+KeyS")
        .build(app)?;
    let terminate_item = MenuItemBuilder::with_id(MENU_TAB_TERMINATE, TERMINATE_TITLE)
        .accelerator("Shift+Cmd+KeyK")
        .build(app)?;
    // Enabled by `sync_agent_menu`, which `rebuild_menu` runs right after this build.
    let agent_new_item = MenuItemBuilder::with_id(MENU_AGENT_NEW, AGENT_NEW_TITLE)
        .accelerator("CmdOrCtrl+KeyT")
        .enabled(false)
        .build(app)?;
    let agent_sleep_item = MenuItemBuilder::with_id(MENU_AGENT_SLEEP, "Sleep Agent Tab")
        .accelerator("Alt+Cmd+KeyS")
        .enabled(false)
        .build(app)?;
    let disabled = |id: &str, title: &str, accel: &str| {
        MenuItemBuilder::with_id(id, title)
            .accelerator(accel)
            .enabled(false)
            .build(app)
    };
    let agent_prev_item = disabled(MENU_AGENT_PREV, "Previous Agent Tab", "Cmd+ArrowUp")?;
    let agent_next_item = disabled(MENU_AGENT_NEXT, "Next Agent Tab", "Cmd+ArrowDown")?;
    let agent_prev_alias = disabled(MENU_AGENT_PREV_ALIAS, ALIAS_TITLES[0], "Cmd+KeyK")?;
    let agent_next_alias = disabled(MENU_AGENT_NEXT_ALIAS, ALIAS_TITLES[1], "Cmd+KeyJ")?;
    let agent_up_item = disabled(
        MENU_AGENT_MOVE_UP,
        "Move Agent Tab Up",
        "Alt+Shift+Cmd+ArrowUp",
    )?;
    let agent_down_item = disabled(
        MENU_AGENT_MOVE_DOWN,
        "Move Agent Tab Down",
        "Alt+Shift+Cmd+ArrowDown",
    )?;
    let close_project_item = MenuItemBuilder::with_id(MENU_TAB_CLOSE_PROJECT, "Close Project Tab")
        .accelerator("Alt+Cmd+KeyW")
        .build(app)?;
    let agent_fork_item = disabled(MENU_AGENT_FORK, "Fork Agent Tab", "Alt+Cmd+KeyT")?;
    let pane_left_item = disabled(MENU_PANE_LEFT, "Focus Left Pane", "Alt+Cmd+ArrowLeft")?;
    let pane_right_item = disabled(MENU_PANE_RIGHT, "Focus Right Pane", "Alt+Cmd+ArrowRight")?;
    let scratch_new_item = disabled(MENU_SCRATCH_NEW, SCRATCH_NEW_TITLE, "Ctrl+Cmd+KeyT")?;
    // Sleep Server ▸ one item per server, enabled and named by `sync_server_menu`; with none, a
    // disabled line says so.
    let server_items = servers
        .into_iter()
        .map(|s| {
            let item = MenuItemBuilder::with_id(format!("{MENU_SLEEP_SERVER}{s}"), s.as_str())
                .enabled(false)
                .build(app)?;
            Ok((s, item))
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let mut server_menu = SubmenuBuilder::new(app, "Sleep Server");
    for (_, item) in &server_items {
        server_menu = server_menu.item(item);
    }
    if server_items.is_empty() {
        let none = MenuItemBuilder::new("No Rex servers")
            .enabled(false)
            .build(app)?;
        server_menu = server_menu.item(&none);
    }
    let server_menu = server_menu.build()?;
    let tab_menu = shell_core::menu::build_tab_menu(
        app,
        &spine,
        mode.is_cycle(),
        &[
            &[
                &split_pane_item,
                &scratch_new_item,
                &pane_left_item,
                &pane_right_item,
            ],
            &[&suspend_item, &terminate_item],
            &[
                &agent_new_item,
                &agent_prev_item,
                &agent_next_item,
                &agent_prev_alias,
                &agent_next_alias,
                &agent_up_item,
                &agent_down_item,
                &agent_fork_item,
                &agent_sleep_item,
                &close_project_item,
                &server_menu,
            ],
        ],
    )?;

    // Reopen Last Closed (⌘⇧T) is warden-only (curator/lector have no equivalent), so it's not
    // part of the spine (YAGNI — add it there only if a sibling app wants it too). Spliced into
    // the spine's already-built Window submenu, at the top, mirroring warden's original layout
    // (reopen-last + separator, then the rest). In an agent pane with a closed agent tab ⌘⇧T
    // reopens that instead; `sync_agent_menu` enables it for either.
    let reopen_last = MenuItemBuilder::with_id(MENU_WINDOW_REOPEN_LAST, "Reopen Last Closed")
        .accelerator("Shift+Cmd+KeyT")
        .enabled(false)
        .build(app)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let window_menu = &spine.submenus[2];
    window_menu.insert_items(&[&reopen_last, &sep], 0)?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &spine.submenus[0], // App
            &spine.edit,
            &tab_menu,
            &spine.submenus[1], // Config
            &spine.submenus[2], // Window
        ])
        .build()?;
    app.set_menu(menu)?;
    // Queued behind `set_menu`'s own main-thread work, so the installed menu is the one walked.
    app.run_on_main_thread(hide_alias_items)?;
    if let Some(keep) = tauri::Manager::try_state::<AgentMenu>(app) {
        *keep.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(AgentItems {
            split: split_pane_item,
            new: agent_new_item,
            sleep: agent_sleep_item,
            reopen: reopen_last,
            prev: [agent_prev_item, agent_prev_alias],
            next: [agent_next_item, agent_next_alias],
            move_up: agent_up_item,
            move_down: agent_down_item,
            close_tab: spine.close_tab.clone(),
            fork: agent_fork_item,
            pane_left: pane_left_item,
            pane_right: pane_right_item,
            scratch_new: scratch_new_item,
            suspend: suspend_item,
            terminate: terminate_item,
        });
    }
    if let Some(keep) = tauri::Manager::try_state::<ServerMenu>(app) {
        *keep.0.lock().unwrap_or_else(|e| e.into_inner()) = server_items;
    }
    Ok(())
}

/// Re-derive the app menu from current manager state and install it. Locks
/// `ManagerState` itself, so callers MUST NOT hold the lock when calling this
/// (the mutex is non-reentrant). Rebuilds on launch, window open/close, and
/// hot-reload — the Window submenu's checkmarks/(closed) tags track live state.
#[cfg(target_os = "macos")]
fn rebuild_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::Manager;
    let mode = app.state::<ManagerState>().lock().last_good.tab_digit_keys;
    let entries = app.state::<WindowSet>().entries();
    build_app_menu(app, mode, entries, rex_servers(app))?;
    sync_agent_menu(app);
    sync_server_menu(app);
    Ok(())
}

/// Hide the [`ALIAS_TITLES`] items in the installed main menu while keeping their key equivalents
/// live. tauri exposes no `NSMenuItem`, so they are found by title (main thread).
#[cfg(target_os = "macos")]
fn hide_alias_items() {
    use objc2_app_kit::{NSApplication, NSMenu};
    use objc2_foundation::MainThreadMarker;
    fn walk(menu: &NSMenu) {
        for item in menu.itemArray().iter() {
            if let Some(sub) = item.submenu() {
                walk(&sub);
            } else if ALIAS_TITLES.contains(&item.title().to_string().as_str()) {
                item.setAllowsKeyEquivalentWhenHidden(true);
                item.setHidden(true);
            }
        }
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu() {
        walk(&menu);
    }
}

/// Return the calling window's banner + tab descriptors, resolved by label. A tab a
/// `warden://` URL named before this call becomes the one the chrome opens on.
#[cfg(target_os = "macos")]
#[tauri::command]
fn init_tabs(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) -> Option<InitDto> {
    let mut m = state.lock();
    let mut dto = m.init_dto(window.label())?;
    if let Some(ws) = m.windows.get_mut(window.label()) {
        ws.chrome_ready = true;
        if let Some(id) = ws.pending_focus.take() {
            dto.active = Some(id);
        }
    }
    Some(dto)
}

/// `warden://` requests held until `setup` has built the window set. A cold launch by URL gets
/// `application:openURLs:` before `applicationDidFinishLaunching`, so `RunEvent::Opened` runs
/// ahead of `setup`; `None` once `setup` has drained it and requests open directly.
#[cfg(target_os = "macos")]
struct EarlyOpens(std::sync::Mutex<Option<Vec<shell_core::windows::OpenRequest>>>);

#[cfg(target_os = "macos")]
impl Default for EarlyOpens {
    fn default() -> Self {
        Self(std::sync::Mutex::new(Some(Vec::new())))
    }
}

/// Open `req` now, or hold it in [`EarlyOpens`] when `setup` hasn't run yet.
#[cfg(target_os = "macos")]
fn open_or_hold(app: &tauri::AppHandle, req: shell_core::windows::OpenRequest) {
    use tauri::Manager;
    let early = app.state::<EarlyOpens>();
    let mut held = early.0.lock().unwrap_or_else(|e| e.into_inner());
    match held.as_mut() {
        Some(queue) => queue.push(req),
        None => {
            drop(held);
            open_url(app, req);
        }
    }
}

/// `warden://open?window=<title>&tab=<id or title>`: open or focus the window, then select the
/// tab. An unknown window or tab is named in an alert.
#[cfg(target_os = "macos")]
fn open_url(app: &tauri::AppHandle, req: shell_core::windows::OpenRequest) {
    use tauri::Manager;
    let Some(title) = req.window.as_deref() else {
        return;
    };
    let Some(label) = app.state::<WindowSet>().label_for_title(title) else {
        surface::link::notice(
            "warden Can't Open That Window",
            &format!("No window is titled \u{201c}{title}\u{201d}."),
        );
        return;
    };
    open_or_focus_window(app, &label);
    let Some(name) = req.param("tab") else { return };
    let ready = {
        let st = app.state::<ManagerState>();
        let mut m = st.lock();
        let Some(ws) = m.windows.get_mut(&label) else {
            return; // it failed to open, which reports itself
        };
        let tabs = ws.registry.tab_dtos();
        let pairs: Vec<_> = tabs
            .iter()
            .map(|t| (t.id.as_str(), t.title.as_str()))
            .collect();
        let Some(id) = manager::tab_named(&pairs, name).map(str::to_string) else {
            drop(m);
            surface::link::notice(
                "warden Can't Open That Tab",
                &format!("\u{201c}{title}\u{201d} has no tab named \u{201c}{name}\u{201d}."),
            );
            return;
        };
        if ws.chrome_ready {
            Some(id)
        } else {
            ws.pending_focus = Some(id);
            None
        }
    };
    if let Some(id) = ready {
        notify::focus_tab(app, &label, &id, None);
    }
}

/// Probe this window's tabs once, on demand. The chrome calls this right after its
/// `warden:session-state` listener is registered, so the first session-presence emit
/// can't be lost to the listener-registration race — which matters most for
/// `probe_interval = 0` (no timer to heal a dropped emit) and also removes the
/// up-to-one-tick hollow-dot latency at startup for every interval.
#[cfg(target_os = "macos")]
#[tauri::command]
fn probe_now(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) {
    use tauri::Manager;
    // The chrome calls this once its `warden:session-state` listener is registered. Two deliveries,
    // together race-proof:
    //  1. Replay the last-known presence from the cache NOW. A probe pass that finished between this
    //     window's build (when `init_dto` snapshotted an empty cache → hollow dots) and the listener
    //     registering had its live per-tab emits dropped, and — `prev` populated — no later pass
    //     re-emits, so those dots would stay dark — for good, on a tab whose session pre-existed and
    //     so never changes state after that lost first pass.
    //  2. Bump so a live burst refreshes anything that changed since.
    let snapshot = state.lock().presence_cache.snapshot(window.label());
    probe::emit_presence_snapshot(window.app_handle(), window.label(), &snapshot);
    probe::bump(window.label());
}

/// What `activate_tab` reports back to the chrome. `live` is load-bearing (see below); `split` and
/// `focused` are the tab-switch path's answer to "does this tab have a second pane, and which one
/// has keyboard focus" — the one place `Registry::activate`'s focus-preserving branch is ever
/// checked (it's untestable in isolation — see the branch's own doc comment). `split` also
/// short-circuits the `focused_pane` lookup below, since an unsplit tab's answer is always the
/// primary.
#[cfg(target_os = "macos")]
#[derive(serde::Serialize)]
struct ActivateResult {
    live: bool,
    split: bool,
    focused: usize,
}

/// Activate tab `id` within the calling window's registry. **`live` reports whether the tab is
/// now live** — i.e. whether a surface actually covers the content hole; `split`/`focused` report
/// this tab's own pane layout (see `ActivateResult`).
///
/// `live` is load-bearing, not informational. warden's window is transparent and the
/// terminal NSView composites *above* the webview, so the hole is only ever filled by a live
/// surface; the chrome's `#empty-state` is the opaque backstop for when it isn't. A failed lazy
/// spawn leaves the tab cold **but still selected**, and the chrome cannot see that from the
/// `warden:error` banner alone — so if this returned nothing, the chrome would keep the placeholder
/// hidden (it keys on "a tab is selected") and the uncovered hole would leak the wallpaper: exactly
/// the leak the placeholder exists to stop. Report liveness so the chrome paints it: a surface
/// standing (`Registry::is_spawned`), so a Rex tab still opening its session, or one an automatic
/// open left with nothing to show, is cold until the `warden:refresh` its landing sends
/// (`rex::spawn`).
///
/// `user` is false when the chrome selected the tab on warden's behalf (window init, the
/// component's fallback after a snapshot): an agent pane then opens as warden's own and wakes
/// nothing (`Registry::activate`). Absent, it is the user's.
#[cfg(target_os = "macos")]
#[tauri::command]
fn activate_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    user: Option<bool>,
) -> ActivateResult {
    use tauri::Emitter;
    // Capture both the spawn outcome AND whether this call spawned a FRESH cold surface (whose
    // `initial_input` may be starting a session) for a probe-enabled tab — the two facts that
    // decide whether to arm a session-start await below. `split`/`focused` are this tab's OWN
    // layout, read under the SAME lock so they can't race a concurrent split/close. Both read
    // under the one lock.
    let (spawned_fresh, has_probe, err, live, split, focused) = {
        let mut m = state.lock();
        let out = match m.windows.get_mut(window.label()) {
            Some(ws) => {
                let has_probe = ws.registry.tab_has_probe(&id);
                let activated = if user.unwrap_or(true) {
                    ws.registry.activate_user(&id)
                } else {
                    ws.registry.activate(&id)
                };
                let (fresh, err) = match activated {
                    Ok(fresh) => (fresh, None),
                    Err(e) => (false, Some(e)),
                };
                let live = err.is_none() && ws.registry.is_spawned(&id);
                let split = ws.registry.is_split(&id);
                // Trivially always the primary when unsplit — no need to ask `focused_pane` at all.
                let focused = if split {
                    ws.registry
                        .focused_pane(&id)
                        .map_or(0, registry::PaneIdx::index)
                } else {
                    0
                };
                (fresh, has_probe, err, live, split, focused)
            }
            None => (false, false, None, false, false, 0),
        };
        m.record_session(tauri::Manager::app_handle(&window), window.label());
        out
    };
    // A lazy spawn failed on click: the tab stays cold (and selected — the chrome paints its
    // empty-state placeholder over the uncovered hole) instead of panicking. The chrome is
    // listening now, so push the reason to the banner.
    if let Some(e) = err {
        log::error!("surface spawn failed for tab {id:?}: {e}");
        // A per-tab spawn error belongs to THIS window only. `emit` broadcasts to every
        // webview (the documented emit_to-leaks footgun), so stamp the window label into
        // the payload and let the chrome's label filter drop it in siblings — mirroring the
        // per-window build-time error (InitDto.error). The global config-error path keeps
        // emitting a bare string (no label), which every window's banner still shows.
        let _ = window.emit(
            "warden:error",
            serde_json::json!({
                "label": window.label(),
                "message": format!("couldn't open terminal: {e}"),
            }),
        );
    }
    // Fast-burst this window so a just-activated tab's dot is current within a pass, probing this tab
    // first so it lands in the first wave. If we just cold-spawned a probe-enabled tab, its
    // `initial_input` may be starting a session asynchronously — arm a session-start await so the
    // burst holds until it comes up (bounded by CAP), instead of settling "absent" within the
    // ~AGREE_TARGET×FAST window and only catching the session on the next slow poll. A warm switch
    // (or a probe-less tab) has no expected transition, so it's a plain priority bump.
    if spawned_fresh && has_probe {
        probe::bump_tab_await(window.label(), &id, probe::Await::Up);
    } else {
        probe::bump_tab(window.label(), &id);
    }
    // `Registry::activate` also brings a split tab's cold SECONDARY live, silently, so the
    // chrome's `secondarySpawnedById` mirror goes stale the moment that secondary spawns here
    // (e.g. re-activating a tab split while backgrounded). Push a fresh snapshot so the mirror
    // catches up, through the same init_dto→emit path every other refresh uses.
    //
    // Gated on `split`: only a tab with a secondary has a mirror entry that can go stale, and an
    // ungated push would cost every ordinary tab click a full snapshot plus a second
    // `applyDto` → `sb.update()` → `setActiveId` → `reportRect` pass on the hottest command in
    // the app.
    if split {
        if let Some(dto) = state.lock().init_dto(window.label()) {
            let _ = window.emit("warden:refresh", dto);
        }
    }
    sync_agent_menu(tauri::Manager::app_handle(&window));
    ActivateResult {
        live,
        split,
        focused,
    }
}

/// Kill tab `id`'s terminal (surface + PTY) in the calling window; it goes cold and
/// respawns fresh on next focus. Returns the id of the tab that became active if the
/// killed one was visible (so the chrome moves its highlight there), else `None`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn unload_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) -> Option<String> {
    let new_active = {
        let mut m = state.lock();
        let new_active = m
            .windows
            .get_mut(window.label())
            .and_then(|ws| ws.registry.unload(&id));
        m.record_session(tauri::Manager::app_handle(&window), window.label());
        new_active
    };
    manager::close_spent_popouts(tauri::Manager::app_handle(&window));
    sync_agent_menu(tauri::Manager::app_handle(&window));
    new_active
}

/// What ⌘D does on a tab with no Rex block focused: reopen the split the config declares, or
/// declare one.
enum SplitAction {
    /// The tab resolves a split: (re)declare its column and bring it live.
    Reopen,
    /// It resolves none: save `split = true` onto this tab's entry in window `0`'s config.
    Save(String, Box<warden_config::Tab>),
}

/// Split tab `id`. The config is the one source of a tab's split:
///
/// - **No split resolved** → `split_write::save_split` writes `split = true` onto the tab's
///   config entry, and the watcher's hot-reload brings the second pane up beside the running
///   primary (`reconcile`'s `split_tabs` → `Registry::split_from_config`). Nothing is split
///   here — the reload is the one path. A failed write (the file broken mid-edit) goes to this
///   window's banner and changes nothing.
/// - **A split resolved but its column closed** (its shell exited) → `Registry::split` re-declares
///   it Cold and `activate_user` spawns and shows it; on an open column the pair is a no-op
///   re-activation, so a stray second ⌘D can't make a third pane. `activate`'s `Err` is the
///   *primary's* spawn failure, so it goes to the banner as a label-stamped `warden:error`,
///   exactly as `activate_tab` reports it.
///
/// `Err` only for `window not found` and a tab that can't split (an unclaimed Rex session's),
/// which the chrome's banner shows.
///
/// With focus in a block of a Rex scratch column, the split is Rex's instead: a bare shell
/// stacked above that block ([`ScratchOp::Split`]), shown when Rex's next view lands.
#[cfg(target_os = "macos")]
#[tauri::command]
fn split_pane(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) -> Result<(), String> {
    split_tab(window, state, id, rex::column::StackSide::Above)
}

/// [`split_pane`], stacking a scratch column's new shell on `side` of its focused block.
#[cfg(target_os = "macos")]
fn split_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    side: rex::column::StackSide,
) -> Result<(), String> {
    use tauri::Emitter;
    let banner = |message: String| {
        let _ = window.emit(
            "warden:error",
            serde_json::json!({ "label": window.label(), "message": message }),
        );
    };
    let block = {
        let m = state.lock();
        let ws = m.windows.get(window.label()).ok_or("window not found")?;
        if ws.registry.focused_rex_block(&id).is_some() {
            // The column stands: a split it can't take yet is the banner's (see below).
            if let Some(why) = ws.registry.column_change_blocked(&id) {
                banner(format!("can't split yet: {why}"));
                return Ok(());
            }
        }
        match split_kind(&ws.registry, &id)? {
            pane_menu::SplitKind::Stack => ws.registry.focused_rex_block(&id),
            pane_menu::SplitKind::Split => None,
        }
    };
    if let Some(block) = block {
        // The column stands whatever happens to the split, so a failure is the banner's — an
        // `Err` here would collapse the chrome's column.
        let app = tauri::Manager::app_handle(&window);
        if let Err(e) = scratch_run(app, window.label(), &id, ScratchOp::Split { block, side }) {
            rex::lifecycle::banner(app, window.label(), &e);
        }
        return Ok(());
    }
    let action = {
        let m = state.lock();
        let ws = m.windows.get(window.label()).ok_or("window not found")?;
        let spec = ws.registry.spec_of(&id).ok_or("tab not found")?;
        if spec.split.is_some() {
            SplitAction::Reopen
        } else {
            let (title, tab) = config_tab(&m, window.label(), &id)?;
            SplitAction::Save(title, Box::new(tab))
        }
    };
    if let SplitAction::Save(title, tab) = action {
        // Outside the lock: the write's own reload takes it.
        if let Err(e) = split_write::save_split(&title, &tab, split_write::SplitEdit::Enable) {
            log::error!("split of tab {id:?} not saved: {e}");
            banner(e);
        }
        return Ok(());
    }
    let primary = {
        let mut m = state.lock();
        let ws = m
            .windows
            .get_mut(window.label())
            .ok_or("window not found")?;
        ws.registry.split(&id);
        ws.registry.activate_user(&id)
    };
    if let Err(e) = primary {
        log::error!("surface spawn failed for tab {id:?}: {e}");
        banner(format!("couldn't open terminal: {e}"));
    }
    // `activate` attempts BOTH panes regardless of the primary's own Result (see its doc
    // comment) — the secondary's spawn outcome is exactly what the chrome's `secondarySpawnedById`
    // mirror needs and can't know on its own (it's best-effort/discarded here, same as anywhere
    // else that outcome is invisible). The `window not found` `Err` returned above, before here;
    // `init_dto` would fail the identical lookup, so there is nothing to refresh for it.
    if let Some(dto) = state.lock().init_dto(window.label()) {
        let _ = window.emit("warden:refresh", dto);
    }
    // The new column enables New Scratch Tab and the pane moves.
    sync_agent_menu(tauri::Manager::app_handle(&window));
    Ok(())
}

/// What ⌘D would do on tab `id`: stack a shell on its focused Rex scratch block, or split the
/// tab — or why it can't (a Rex session opened from the sidebar has no config entry to save a
/// split to). [`split_pane`] acts on it and the right-click menu shows it.
#[cfg(target_os = "macos")]
fn split_kind(reg: &registry::Registry, id: &str) -> Result<pane_menu::SplitKind, String> {
    if reg.focused_rex_block(id).is_some() {
        if let Some(why) = reg.column_change_blocked(id) {
            return Err(why.to_owned());
        }
        return Ok(pane_menu::SplitKind::Stack);
    }
    let spec = reg.spec_of(id).ok_or("tab not found")?;
    if spec.split.is_none()
        && rex::unclaimed::is_unclaimed(&spec.id, &spec.dir, spec.server.as_deref())
    {
        return Err("a Rex session opened from the sidebar can't split".into());
    }
    Ok(pane_menu::SplitKind::Split)
}

/// Tab `id` of window `label` as the loaded config has it, with its config window's title (a
/// pop-out's origin's): what a split write addresses.
#[cfg(target_os = "macos")]
fn config_tab(
    m: &manager::WindowManager,
    label: &str,
    id: &str,
) -> Result<(String, warden_config::Tab), String> {
    let title = m.config_title(label).ok_or("window not found")?;
    m.last_good
        .windows
        .iter()
        .find(|w| w.title == title)
        .and_then(|w| w.tabs.iter().find(|t| t.key == id))
        .map(|t| (title.to_owned(), t.clone()))
        .ok_or_else(|| format!("no config entry for tab {id:?}"))
}

/// A scratch block's share of its innermost run, as the page shows it.
#[cfg(target_os = "macos")]
#[derive(serde::Deserialize)]
struct PaneShare {
    axis: pane_menu::Axis,
    share: f64,
}

/// The page's answer to `warden:pane-menu`: pop the right-click menu for pane `pane` (its wire
/// index) of tab `id` at `(x, y)` in this window. `size` is the second pane's share the page
/// shows — a divider drag included — or `None` while the tab shows no split. `pane_share` is a
/// scratch block's share of its run (`None` on the main pane); `only_pane`, that the block is its
/// column's only one.
#[cfg(target_os = "macos")]
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn pane_menu(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    pane: usize,
    x: f64,
    y: f64,
    size: Option<f64>,
    pane_share: Option<PaneShare>,
    only_pane: Option<bool>,
) -> Result<(), String> {
    let pane = registry::PaneIdx::from_index(pane);
    let pane_size = match pane {
        registry::PaneIdx::Primary => None,
        registry::PaneIdx::Block(_) if only_pane == Some(true) => {
            Some(Err(pane_menu::ONLY_PANE.to_owned()))
        }
        registry::PaneIdx::Block(_) => pane_share.map(|s| {
            Ok(pane_menu::PaneSizeState {
                axis: s.axis,
                current: s.share,
            })
        }),
    };
    let label = window.label().to_owned();
    let (has_selection, can_split, close_pane, tab) = {
        let m = state.lock();
        let ws = m.windows.get(&label).ok_or("window not found")?;
        let has_selection = m
            .pane_surface(&label, &id, pane)
            .is_some_and(|s| s.has_selection());
        let tab = size.map(|_| config_tab(&m, &label, &id));
        let close_pane = ws.registry.close_pane_item(&id);
        (
            has_selection,
            split_kind(&ws.registry, &id),
            close_pane,
            tab,
        )
    };
    // Read outside the lock: it parses the file.
    let split = size.zip(tab).map(|(current, tab)| {
        tab.and_then(|(title, tab)| split_write::current_inherited_size(&title, &tab))
            .map(|inherited| pane_menu::SizeState { current, inherited })
            .inspect_err(|e| log::warn!("pane menu of tab {id:?}: Size disabled: {e}"))
    });
    let ctx = pane_menu::PaneMenuCtx {
        label,
        tab: id,
        pane,
        has_selection,
        can_split,
        split,
        close_pane,
        pane_size,
    };
    pane_menu::popup(tauri::Manager::app_handle(&window), &ctx, x, y)
}

/// A right-click menu item was chosen.
#[cfg(target_os = "macos")]
fn on_pane_menu(app: &tauri::AppHandle, target: pane_menu::Target) {
    use pane_menu::{Op, PRESETS};
    use tauri::{Emitter, Manager};
    let pane_menu::Target {
        op,
        pane,
        label,
        tab,
    } = target;
    let (label, tab) = (label.as_str(), tab.as_str());
    let banner = |message: String| {
        let _ = app.emit_to(
            label,
            "warden:error",
            serde_json::json!({ "label": label, "message": message }),
        );
    };
    let state = app.state::<ManagerState>();
    match op {
        Op::Copy | Op::Paste => {
            let m = state.lock();
            match (m.pane_surface(label, tab, pane), op) {
                (Some(s), Op::Copy) => s.copy(),
                (Some(s), _) => s.paste(),
                (None, _) => {}
            }
        }
        Op::Split | Op::SplitBelow => {
            let Some(window) = app.get_webview_window(label) else {
                return;
            };
            let side = match op {
                Op::SplitBelow => rex::column::StackSide::Below,
                _ => rex::column::StackSide::Above,
            };
            if let Err(e) = split_tab(window, state, tab.to_owned(), side) {
                banner(e);
            }
        }
        Op::ClosePane => {
            if let Err(e) = close_pane(app, label, tab, pane) {
                banner(e);
            }
        }
        Op::SizeUnavailable | Op::PaneSizeUnavailable => {}
        Op::Size(i) => save_tab_split(app, label, tab, split_write::SplitEdit::Size(PRESETS[i])),
        Op::SizeDefault => save_tab_split(app, label, tab, split_write::SplitEdit::ResetSize),
        // The page resizes the block's run as a drag's release does.
        Op::PaneSize(i) => {
            let block = state
                .lock()
                .windows
                .get(label)
                .and_then(|ws| ws.registry.rex_block_id(tab, pane));
            match block {
                Some(block_id) => {
                    let _ = app.emit_to(
                        label,
                        "warden:scratch-size",
                        serde_json::json!({
                            "label": label, "id": tab, "blockId": block_id,
                            "share": pane_menu::PANE_PRESETS[i],
                        }),
                    );
                }
                None => log::warn!("pane size of tab {tab:?}: pane {pane:?} is not a Rex block"),
            }
        }
    }
}

/// Write a size edit to tab `tab`'s config entry, tell the page the stored size, and banner a
/// failure. The page drops a dragged ratio for the stored size; the reload carries it to the
/// file's `size` too, which a size equal to the old one never changes.
#[cfg(target_os = "macos")]
fn save_tab_split(app: &tauri::AppHandle, label: &str, tab: &str, edit: split_write::SplitEdit) {
    use tauri::{Emitter, Manager};
    let state = app.state::<ManagerState>();
    let saved = config_tab(&state.lock(), label, tab).and_then(|(title, t)| {
        let size = match edit {
            split_write::SplitEdit::Size(s) => split_write::stored_size(s),
            _ => split_write::current_inherited_size(&title, &t)?,
        };
        split_write::save_split(&title, &t, edit).map(|()| size)
    });
    match saved {
        Ok(size) => {
            let _ = app.emit_to(
                label,
                "warden:split-size",
                serde_json::json!({ "label": label, "id": tab, "size": size }),
            );
        }
        Err(e) => {
            log::error!("split size of tab {tab:?} not saved: {e}");
            rex::lifecycle::banner(app, label, &e);
        }
    }
}

/// Save the second pane's share of tab `id`'s split (a divider drag's release) to the config.
#[cfg(target_os = "macos")]
#[tauri::command]
fn save_split_size(window: tauri::WebviewWindow, id: String, size: f64) -> Result<(), String> {
    use warden_config::resolve::{SPLIT_SIZE_MAX, SPLIT_SIZE_MIN};
    let app = tauri::Manager::app_handle(&window);
    save_tab_split(
        app,
        window.label(),
        &id,
        split_write::SplitEdit::Size(size.clamp(SPLIT_SIZE_MIN, SPLIT_SIZE_MAX)),
    );
    Ok(())
}

/// Make this window's webview the key view, taking first responder from its terminal so the
/// chrome can focus its own field. The page can't take focus from AppKit itself.
#[cfg(target_os = "macos")]
#[tauri::command]
fn focus_sidebar(window: tauri::WebviewWindow) {
    let _ = AsRef::<tauri::Webview<_>>::as_ref(&window).set_focus();
}

/// Runs a spine command in the window `shell_core::menu::route` chose. Every emit is
/// label-stamped and the chrome's forMe() filter drops the rest: `emit_to` leaks to sibling
/// webviews, so without the stamp one menu click would act in every open window.
#[cfg(target_os = "macos")]
fn on_spine_action(app: &tauri::AppHandle, routed: shell_core::menu::Routed) {
    use shell_core::menu::{SpineAction, TabNavAction};
    use tauri::{Emitter, Manager};
    let label = routed.target.as_str();
    let target = app.get_webview_window(label);
    if routed.from_detached.is_some() {
        match &target {
            Some(w) => {
                let _ = w.set_focus();
            }
            // The pop-out's origin was closed: bring it back. Its chrome mounts fresh, with nothing
            // listening yet for this command, so the command ends there.
            None => {
                open_or_focus_window(app, label);
                return;
            }
        }
    }
    // A popped-out tab never holds its origin's highlight, so Next/Previous step from it.
    let from = routed.from_detached.as_deref().and_then(|l| {
        let st = app.state::<ManagerState>();
        let m = st.lock();
        Some(m.windows.get(l)?.origin.as_ref()?.tab_id.clone())
    });
    let emit = |event: &str| {
        let _ = app.emit_to(label, event, serde_json::json!({ "label": label }));
    };
    match routed.action {
        SpineAction::Nav(TabNavAction::Next | TabNavAction::Prev) => {
            let dir = if routed.action == SpineAction::Nav(TabNavAction::Next) {
                1
            } else {
                -1
            };
            let _ = app.emit_to(
                label,
                "warden:cycle-tab",
                serde_json::json!({ "label": label, "dir": dir, "from": from }),
            );
        }
        SpineAction::Nav(TabNavAction::Jump(n)) => {
            let _ = app.emit_to(
                label,
                "warden:select-tab",
                serde_json::json!({ "label": label, "n": n }),
            );
        }
        // ⌘W unloads the active tab (kill surface+PTY → cold, respawns on next focus); it does NOT
        // close the window. The chrome owns "which tab is active" + the dot/highlight repaint.
        SpineAction::CloseTab => emit("warden:unload-tab"),
        // Closing a pop-out docks its tab (⌘W there closes a selected agent tab first — the
        // menu handler's `agent_closable`); closing a window reaps its surfaces, then the
        // `WindowSet` shows the home surface if it was the last real window — no quit.
        SpineAction::DockPoppedOut | SpineAction::CloseWindow => {
            if let Some(w) = target {
                let _ = w.close();
            }
        }
        SpineAction::PopOutTab => emit("warden:pop-out-tab"),
        // ⌘⇧F: a terminal NSView holds first responder — make the webview the key view first,
        // then the chrome focuses its field.
        SpineAction::FindInSidebar => {
            if let Some(w) = target {
                focus_sidebar(w);
            }
            emit("warden:find-in-sidebar");
        }
        SpineAction::ToggleSidebar => emit("warden:toggle-sidebar"),
        // The chrome runs a manual check whatever auto_update says.
        SpineAction::CheckUpdates => emit("warden:check-update"),
    }
}

/// Point tab `id`'s keyboard focus at pane `pane` (0 = primary, 1 = secondary). Returns false
/// (no-op) if that pane doesn't exist — e.g. a stray click on a secondary that just closed.
#[cfg(target_os = "macos")]
#[tauri::command]
fn focus_pane(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    pane: usize,
) -> bool {
    let which = registry::PaneIdx::from_index(pane);
    let mut m = state.lock();
    match m.windows.get_mut(window.label()) {
        Some(ws) => ws.registry.focus_pane(&id, which),
        None => false,
    }
}

/// A pop-out's window opens at this size the FIRST time that tab is popped — one tab needs far
/// less than a full multi-tab window's config-resolved default (1500×1000).
///
/// Only the first time: shell-core's geometry plugin persists a pop-out's size and position keyed
/// on its (deterministic, per-tab) `shell-detach:` label, and restores over this size inside
/// `open_detached`'s `build()`. So this pair is the no-memory default, never how big the window
/// will be: the birth rect in `pop_out_tab` is sized from the size `open_detached` hands it.
#[cfg(target_os = "macos")]
const POPOUT_DEFAULT_WIDTH: f64 = 900.0;
#[cfg(target_os = "macos")]
const POPOUT_DEFAULT_HEIGHT: f64 = 640.0;

/// Pop tab `id` out of the calling window into a window of its own, terminals (surfaces + PTYs)
/// intact: a one-tab window under a `shell-detach:` label that serves warden's own page, its
/// registry holding the whole tab — agents, scratch column and all
/// ([`registry::Registry::take_tab`]). A placeholder keeps the tab's row in the origin until the
/// pop-out closes, when `redock` (wired via `shell_core::detach::wire_return`) takes the tab home.
///
/// A cold tab is brought up first, so the ⤢ on any row works — an agent pane whose agents all
/// sleep goes out open on its list, to be woken there. A Rex tab whose main pane or scratch column
/// is still waiting on Rex can't move: the answer lands in the window that asked.
///
/// **Lock discipline (load-bearing):** `ManagerState` is held only to take the tab out (phase 1)
/// and to register the pop-out (phase 3). `open_detached` builds a window and re-enters AppKit, so
/// it runs with the lock RELEASED, its `birth_content` moving the tab into a fresh registry.
#[cfg(target_os = "macos")]
#[tauri::command]
fn pop_out_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) -> Result<(), String> {
    use tauri::{Emitter, Manager};
    let app = window.app_handle().clone();
    let origin_label = window.label().to_string();

    // Phase 1 — under the lock: bring the tab up and take it out, leaving its placeholder.
    let (moved, spec, origin) = {
        let mut m = state.lock();
        let ws = m
            .windows
            .get_mut(&origin_label)
            .ok_or_else(|| "window not found".to_string())?;
        if ws.origin.is_some() {
            return Err("this tab is already popped out".to_string());
        }
        // A Rex open or column change in flight answers the window that asked, so the tab can't
        // move until it lands. Asked before the tab is brought up, so a refusal wakes nothing it
        // can already see; the main pane again after, for the open bringing a cold Rex tab up
        // starts. A column open that bring-up starts again travels: the move forgets it and the
        // pop-out asks afresh, so a column whose open failed never holds the tab back.
        let connecting = |reg: &registry::Registry| {
            reg.is_connecting(&id).then(|| {
                "the tab is still connecting to its Rex server — pop it out once it opens"
                    .to_string()
            })
        };
        let column = |reg: &registry::Registry| {
            reg.column_in_flight(&id)
                .map(|why| format!("{why} — pop the tab out once its scratch column settles"))
        };
        if let Some(why) = connecting(&ws.registry).or_else(|| column(&ws.registry)) {
            return Err(why);
        }
        ws.registry
            .ensure_spawned_by_id(&id)
            .map_err(|e| format!("could not open the tab to pop it out: {e}"))?;
        if let Some(why) = connecting(&ws.registry) {
            return Err(why);
        }
        let moved = ws
            .registry
            .take_tab(&id)
            .ok_or_else(|| "tab is not available to pop out".to_string())?;
        if !moved.is_up() {
            // Nothing to show once out: it stays where it was.
            if let Err(mut back) = ws.registry.adopt_tab(moved) {
                back.close();
            }
            return Err("tab has nothing running to pop out".to_string());
        }
        let spec = shell_core::detach::DetachSpec {
            title: moved.spec().title.clone(),
            colour: Some(ws.colour.clone()),
            width: POPOUT_DEFAULT_WIDTH,
            height: POPOUT_DEFAULT_HEIGHT,
            page: Some(tauri::WebviewUrl::App("index.html".into())),
        };
        let origin = manager::PopOrigin {
            label: origin_label.clone(),
            title: ws.title.clone(),
            tab_id: moved.id().to_owned(),
        };
        // The moved tab's surfaces still sit in the origin until birth reparents them; a moved
        // tab is never dropped live, so every path below hands it on or closes it.
        (Some(moved), spec, origin)
    };
    let mut moved = moved;

    // Phase 2 — lock RELEASED: build the window; birth_content moves the tab into its registry,
    // framed at the hole below the page's banner until the page reports its own holes. `size` is
    // the BUILT window's real size — shell-core's geometry plugin has restored a remembered one by
    // now — in logical points, which is what the NSView frame wants.
    let token = crate::plan::detach_window_token(&origin_label, &id);
    let mut popped: Option<registry::Registry> = None;
    let build = shell_core::detach::open_detached(&app, &token, &spec, "warden", |win, size| {
        let nsw = win.ns_window()? as *mut std::os::raw::c_void;
        let hole = shell_core::detach::birth_hole(size);
        let birth = geometry::web_rect_to_view(
            WebRect {
                x: hole.x,
                y: hole.y,
                width: hole.width,
                height: hole.height,
            },
            size.height,
        );
        let mut reg = registry::Registry::new(nsw, birth);
        reg.set_rex_opener(rex::spawn::opener(app.clone(), win.label().to_owned()));
        reg.set_report_socket(rex::reports::socket_path(&app));
        let Some(tab) = moved.take() else {
            return Err(std::io::Error::other("the tab is already gone").into());
        };
        match reg.adopt_tab(tab) {
            Ok(()) => {
                popped = Some(reg);
                Ok(())
            }
            Err(back) => {
                moved = Some(back);
                Err(std::io::Error::other("its terminals won't move").into())
            }
        }
    });

    let label = match (build, popped) {
        (Ok(label), Some(reg)) => {
            let mut m = state.lock();
            m.add_popout(
                &app,
                &label,
                reg,
                spec.title.clone(),
                spec.colour.clone().unwrap_or_default(),
                origin,
            );
            label
        }
        (build, _) => {
            // Nothing was built, or the tab wouldn't move: it goes back where it was, live (a
            // surface that moved went back to the origin hidden — the adopt re-shows it).
            let e = build
                .err()
                .map_or_else(|| "no window".to_owned(), |e| e.to_string());
            let mut m = state.lock();
            if let Some(mut tab) = moved.take() {
                match m.windows.get_mut(&origin_label) {
                    Some(ws) => {
                        if let Err(mut back) = ws.registry.adopt_tab(tab) {
                            back.close();
                        }
                    }
                    None => tab.close(),
                }
            }
            m.record_session(&app, &origin_label);
            log::error!("couldn't pop out tab {:?}: {e}", spec.title);
            return Err(format!("couldn't pop out tab: {e}"));
        }
    };
    log::info!("tab {:?}: popped out of {origin_label:?}", spec.title);

    {
        let app2 = app.clone();
        let label2 = label.clone();
        // wire_return resolves the window by label itself (`get_window`).
        shell_core::detach::wire_return(&app, &label, move || redock(&app2, &label2));
    }
    // The origin's row now renders popped out; refresh it.
    state.lock().record_session(&app, &origin_label);
    let _ = rebuild_menu(&app);
    if let Some(dto) = state.lock().init_dto(&origin_label) {
        let _ = app.emit_to(origin_label.as_str(), "warden:refresh", dto);
    }
    Ok(())
}

/// Raise the pop-out holding tab `id` (popped out of the calling window). The chrome calls this
/// instead of `activate_tab` when a popped-out row is clicked in the origin sidebar — there is no
/// local surface to activate, so "select" means "bring its window forward".
#[cfg(target_os = "macos")]
#[tauri::command]
fn raise_popped_window(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
) {
    use tauri::Manager;
    let app = window.app_handle().clone();
    let label = state
        .lock()
        .popout_of(window.label(), &id)
        .map(str::to_owned);
    if let Some(label) = label {
        if let Some(win) = app.get_webview_window(&label) {
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    }
}

/// Dock tab `id` back into its origin window — the ↩ pop-in overlay on a popped-out row. Closing
/// the pop-out fires its `Destroyed` handler (`redock`), which returns the tab here: the same
/// return path as the user closing the pop-out by hand. No-op if `id` isn't a tab popped out of the
/// calling window.
#[tauri::command]
fn pop_in_tab(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    use tauri::Manager;
    let app = window.app_handle().clone();
    let label = state
        .lock()
        .popout_of(window.label(), &id)
        .map(str::to_owned);
    if let Some(label) = label {
        if let Some(win) = app.get_webview_window(&label) {
            let _ = win.close();
        }
    }
}

/// Return a pop-out's tab to its origin when the pop-out closes — the `on_close` wired by
/// `shell_core::detach::wire_return`. Runs on the main thread (Tauri delivers `Destroyed` there),
/// so it can lock `ManagerState` and touch AppKit directly.
#[cfg(target_os = "macos")]
fn redock(app: &tauri::AppHandle, label: &str) {
    use tauri::{Emitter, Manager};
    // App is quitting (⌘Q, `RunEvent::ExitRequested` fires before every window's `Destroyed`):
    // don't reopen an origin window or move a surface mid-teardown — everything is being torn
    // down and `GhosttySurface`'s `Drop` frees it. Without this, a pop-out whose origin was
    // already closed would build a fresh window during app termination.
    if manager::is_quitting(app) {
        return;
    }
    let st = app.state::<ManagerState>();
    // A closed origin is reopened first, while the pop-out is still registered: that is how
    // `build_window` knows to stand the tab's placeholder rather than add it cold — an eager tab
    // (`load_on_open`, remembered, or the first shown) would otherwise spawn a second live
    // terminal, and the return would find no slot. The lock is released: the `WindowSet` builds
    // through `WardenHost`, which takes it.
    let Some(target) = st.lock().origin_home(label) else {
        return; // already returned (double close) — nothing to do
    };
    if let manager::ReturnTarget::Reopen(title) = target {
        reopen_by_title(app, &title);
    }
    let redocked = st.lock().redock(app, label);
    let Some(manager::Redocked {
        origin_label,
        dropped,
    }) = redocked
    else {
        return;
    };
    log::info!(
        "{label:?} docked back into {origin_label:?}{}",
        if dropped.is_some() {
            " (nothing running)"
        } else {
            ""
        }
    );
    // A tab that came home with nothing running goes through the chrome's unload tail BEFORE the
    // refresh, so the selection lands on the neighbour the registry leaned to rather than
    // chrome-core's first-tab fallback.
    if let Some((id, new_active)) = dropped {
        let _ = app.emit_to(
            origin_label.as_str(),
            "warden:tab-exited",
            serde_json::json!({ "label": origin_label, "id": id, "newActive": new_active }),
        );
    }
    // The origin may have been reopened; rebuild the menu so its checkmark/(closed) tag is right
    // (lock already released — rebuild_menu re-locks).
    let _ = rebuild_menu(app);
    // A fresh snapshot: the origin's chrome shows the returned row docked. A no-op emit if the tab
    // had no home.
    let dto = st.lock().init_dto(&origin_label);
    if let Some(dto) = dto {
        let _ = app.emit_to(origin_label.as_str(), "warden:refresh", dto);
    }
    // The tab's session may have changed while out — fast-burst the origin's dots.
    probe::bump(&origin_label);
}

/// End the *session* tab `id` represents (the thing its `probe` checks for) by running its
/// configured `suspend` or `kill` command (per `how`) via `sh -c`, cwd = the tab's dir — or, for a
/// Rex tab's destroy, by destroying its panes' Rex sessions (`rex::spawn::destroy`) —
/// fire-and-forget on a detached thread (exit code ignored — warden has no response to a failed
/// end, and must not block the UI thread). Touches no surface: the chrome unloads the tab itself —
/// except a Rex tab's ⏻ (`rex::lifecycle::sleep_project`), which may stop at a confirm and lets
/// the tab go only once it proceeds, so cancelling leaves the tab as it was. Returns whether warden
/// takes the tab cold itself. No-op if the tab has no command for `how`. After the command
/// completes, bump this window so the scheduler fast-bursts and the presence dot settles promptly
/// once the session is actually gone. Same minimal-env PATH footgun as probes — see scrub note +
/// CLAUDE.md.
#[cfg(target_os = "macos")]
#[tauri::command]
fn end_session(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    how: registry::EndSession,
) -> bool {
    let target = {
        let m = state.lock();
        m.windows
            .get(window.label())
            .and_then(|ws| ws.registry.end_target(&id, how))
    };
    let label = window.label().to_string();
    if target.is_some() {
        log::info!("{how:?} tab {id:?} in {label:?}");
    }
    let (dir, title, cmd) = match target {
        None => return false, // unknown tab or no command configured for `how`
        Some(registry::EndTarget::Rex(pane_keys)) => {
            use tauri::Manager;
            rex::spawn::destroy(window.app_handle(), label, id, pane_keys);
            return false;
        }
        Some(registry::EndTarget::RexSleep { .. }) => {
            use tauri::Manager;
            return match rex::lifecycle::sleep_project(window.app_handle(), &label, &id, &[]) {
                Ok(()) => true,
                Err(e) => {
                    rex::lifecycle::banner(window.app_handle(), &label, &e);
                    false
                }
            };
        }
        Some(registry::EndTarget::Command(dir, title, cmd)) => (dir, title, cmd),
    };
    // Run the kill fire-and-forget off the UI thread, then bump this window so the scheduler's fast
    // burst catches the transition the moment teardown lands. `bump_tab_await` arms a directional
    // await — a suspend should leave Present (onto the ghost), a destroy should reach Absent (a ☠ on
    // a ghost discards it) — so the burst holds Fast until it does rather than settling on the old
    // state and dropping to the slow poll — the fix for the ~5s-lingering dot when `amux --kill`
    // returns before tmux teardown fully lands. No optimistic drop and no suppression flag: the dot
    // tracks `warden:session-state` and converges monotonically (no off→on→off flicker). A
    // genuinely-failed kill leaves the session where it was, so the await simply rides to CAP and
    // the dot correctly stays lit.
    let want = match how {
        registry::EndSession::Suspend => probe::Await::Down,
        registry::EndSession::Destroy => probe::Await::Gone,
    };
    std::thread::spawn(move || {
        probe::run_end(&cmd, &dir, &title);
        // Probe THIS tab first in the burst so its dot clears within one probe, not after every
        // other tab in a wide window's sweep.
        probe::bump_tab_await(&label, &id, want);
    });
    false
}

/// Restart the *session* tab `id` represents by re-typing its startup `cmd` into the live shell
/// (see `Registry::start_session` — the runtime twin of spawn-time `initial_input`). No-op if the
/// tab is cold or has no `cmd`. The started session appears **asynchronously** — the shell has to
/// run the typed command — so a single ordinary burst can settle "absent" before it comes up. A
/// directional await (`bump_tab_await(Await::Up)`, session should come UP) holds the burst Fast until the
/// session is actually `Present` (bounded by CAP), so the dot lights within a probe of the session
/// landing rather than up to a slow poll later. Same minimal-env PATH footgun as probes.
#[cfg(target_os = "macos")]
#[tauri::command]
fn start_session(window: tauri::WebviewWindow, state: tauri::State<ManagerState>, id: String) {
    let started = {
        let m = state.lock();
        m.windows
            .get(window.label())
            .map(|ws| ws.registry.start_session(&id))
            .unwrap_or(false)
    };
    if !started {
        return; // cold / no cmd / unknown — nothing typed, nothing to re-probe
    }
    // Probe this tab first in the burst so its dot lights as soon as the session comes up, not after
    // the rest of a wide window's sweep; the await keeps the burst alive across the async start.
    probe::bump_tab_await(window.label(), &id, probe::Await::Up);
}

// The agent tab list's commands (`rex::lifecycle`). Each answers a refusal at once; Rex work runs
// on a worker, its failure goes to the window's error banner, and `warden:agents` follows.

/// Tab `tab_id`'s agent tab list as it stands, for a chrome that has none for it yet; every change
/// after this arrives as `warden:agents`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agents_init(
    window: tauri::WebviewWindow,
    tab_id: String,
) -> Option<rex::lifecycle::AgentListDto> {
    use tauri::Manager;
    rex::lifecycle::current_list(window.app_handle(), &tab_id)
}

/// The page shows tab `tab_id`'s agent tab `record_id` selected, finished at stamp `at` (its row's
/// `unseen`): seen, while this window is focused. An unfocused window's page asks again when it
/// regains focus (`warden:tab-seen`).
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_seen(window: tauri::WebviewWindow, tab_id: String, record_id: String, at: String) {
    use tauri::Manager;
    if window.is_focused().unwrap_or(false) {
        rex::lifecycle::seen(window.app_handle(), &tab_id, &record_id, &at);
    }
}

/// A new agent tab in tab `tab_id`'s main pane, typing the tab's `cmd`, selected.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_new(window: tauri::WebviewWindow, tab_id: String) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::run(
        window.app_handle(),
        window.label(),
        &tab_id,
        rex::lifecycle::AgentOp::New,
    )
}

/// Show agent tab `record_id`, waking it when it is asleep.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_select(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::select(window.app_handle(), window.label(), &tab_id, &record_id)
}

/// Put agent tab `record_id` to sleep; refused while it hasn't reported how to resume.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_sleep(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
) -> Result<(), String> {
    use tauri::Manager;
    let op = rex::lifecycle::AgentOp::Sleep(record_id);
    rex::lifecycle::run(window.app_handle(), window.label(), &tab_id, op)
}

/// Fork agent tab `record_id`: a new tab below it resuming the same conversation as a branch.
/// Refused while it hasn't reported how to resume, or its harness can't fork.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_fork(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
) -> Result<(), String> {
    use tauri::Manager;
    let op = rex::lifecycle::AgentOp::Fork(record_id);
    rex::lifecycle::run(window.app_handle(), window.label(), &tab_id, op)
}

/// Wake asleep agent tab `record_id`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_wake(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
) -> Result<(), String> {
    use tauri::Manager;
    let op = rex::lifecycle::AgentOp::Wake(record_id);
    rex::lifecycle::run(window.app_handle(), window.label(), &tab_id, op)
}

/// Close agent tab `record_id` into the closed fold. One that never reported how to resume is
/// refused (the chrome then confirms it goes for good) unless `confirmed`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_close(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
    confirmed: bool,
) -> Result<(), String> {
    use tauri::Manager;
    let op = rex::lifecycle::AgentOp::Close {
        id: record_id,
        confirmed,
    };
    rex::lifecycle::run(window.app_handle(), window.label(), &tab_id, op)
}

/// Reopen closed agent tab `record_id`, the most recently closed when absent.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_reopen(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: Option<String>,
) -> Result<(), String> {
    use tauri::Manager;
    let op = rex::lifecycle::AgentOp::Reopen(record_id);
    rex::lifecycle::run(window.app_handle(), window.label(), &tab_id, op)
}

/// Name agent tab `record_id`; a blank name gives a live tab Rex's default.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_rename(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
    name: String,
) -> Result<(), String> {
    use tauri::Manager;
    let op = rex::lifecycle::AgentOp::Rename(record_id, name);
    rex::lifecycle::run(window.app_handle(), window.label(), &tab_id, op)
}

/// Move agent tab `record_id`, with its forks, into `group` (absent or blank: ungrouped; a new
/// name makes the group) at `index` among that group's top-level tabs, last when absent. A fork
/// moved here leaves its family.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_group(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
    group: Option<String>,
    index: Option<usize>,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::edit(window.app_handle(), &tab_id, |s| {
        s.place(
            &tab_id,
            &record_id,
            rex::agents::Place::Root { group, index },
        )
    })
}

/// Move agent tab `record_id`, with its forks, to `index` among its siblings.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_reorder(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
    index: usize,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::edit(window.app_handle(), &tab_id, |s| {
        s.place(&tab_id, &record_id, rex::agents::Place::Sibling(index))
    })
}

/// Fold or unfold the forks under agent tab `record_id`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_fold_family(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: String,
    folded: bool,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::edit(window.app_handle(), &tab_id, |s| {
        s.set_family_folded(&tab_id, &record_id, folded)
    })
}

/// Fold or unfold tab `tab_id`'s agent group `group`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_fold(
    window: tauri::WebviewWindow,
    tab_id: String,
    group: String,
    folded: bool,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::edit(window.app_handle(), &tab_id, |s| {
        s.set_folded(&tab_id, &group, folded);
        true
    })
}

/// Clear closed agent tab `record_id` from the fold, or every closed one when absent.
#[cfg(target_os = "macos")]
#[tauri::command]
fn agent_clear_closed(
    window: tauri::WebviewWindow,
    tab_id: String,
    record_id: Option<String>,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::edit(window.app_handle(), &tab_id, |s| {
        s.clear_closed(&tab_id, record_id.as_deref()) || record_id.is_none()
    })
}

// The scratch column's commands (`ui/index.html`'s `#scratch-tabs` strip and dividers). Showing a
// scratch tab is warden's own; every other change is Rex's, made on a worker ([`scratch_run`]).

/// Show scratch tab `tab` of tab `id`'s column. False when the column has no such tab.
#[cfg(target_os = "macos")]
#[tauri::command]
fn scratch_select(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    tab: usize,
) -> bool {
    use tauri::Emitter;
    let (sizes, dto) = {
        let mut m = state.lock();
        let Some(sizes) = m
            .windows
            .get_mut(window.label())
            .and_then(|ws| ws.registry.select_scratch(&id, tab))
        else {
            return false;
        };
        (sizes, m.init_dto(window.label()))
    };
    if let Some(dto) = dto {
        let _ = window.emit("warden:refresh", dto);
    }
    let app = tauri::Manager::app_handle(&window);
    request_rex_sizes(app, sizes);
    sync_agent_menu(app);
    true
}

/// A new scratch tab in tab `id`'s Rex column: one bare shell in the tab's dir, selected.
#[cfg(target_os = "macos")]
#[tauri::command]
fn scratch_new(window: tauri::WebviewWindow, id: String) -> Result<(), String> {
    let app = tauri::Manager::app_handle(&window);
    scratch_run(app, window.label(), &id, ScratchOp::NewTab)
}

/// Close scratch tab `index` of tab `id`'s Rex column — the strip's × — and every pane in it; the
/// column's last tab takes the split with it.
#[cfg(target_os = "macos")]
#[tauri::command]
fn scratch_close_tab(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    id: String,
    index: usize,
) -> Result<(), String> {
    let window_id = {
        let m = state.lock();
        let ws = m.windows.get(window.label()).ok_or("window not found")?;
        ws.registry
            .scratch_window(&id, index)
            .ok_or("that scratch tab is no longer open")?
    };
    let app = tauri::Manager::app_handle(&window);
    scratch_run(app, window.label(), &id, ScratchOp::CloseTab(window_id))
}

/// One divider of a scratch column and the ratio of its split it moves to.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SplitRatio {
    split_id: String,
    ratio: f64,
}

/// Move dividers `splits` of the scratch tab that tab `id`'s column shows, in order. `sizes`: the
/// column's pane shares top to bottom, saved to the tab's config entry when the shown scratch tab
/// is the one its stack opened as.
#[cfg(target_os = "macos")]
#[tauri::command]
fn scratch_resize(
    window: tauri::WebviewWindow,
    id: String,
    splits: Vec<SplitRatio>,
    sizes: Option<Vec<f64>>,
) -> Result<(), String> {
    let app = tauri::Manager::app_handle(&window);
    let op = ScratchOp::Resize {
        splits: splits
            .into_iter()
            .map(|s| (s.split_id, rex::layout::clamp_ratio(s.ratio)))
            .collect(),
        sizes,
    };
    scratch_run(app, window.label(), &id, op)
}

/// A change Rex makes to a tab's scratch column.
#[derive(Debug)]
enum ScratchOp {
    /// A new window with one bare shell — a scratch tab — selected once made.
    NewTab,
    /// A bare shell stacked on `side` of `block`.
    Split {
        block: String,
        side: rex::column::StackSide,
    },
    /// Dividers of the shown scratch tab, each moved to its ratio, in order; `sizes` the column's
    /// pane shares to save.
    Resize {
        splits: Vec<(String, f64)>,
        sizes: Option<Vec<f64>>,
    },
    /// Rex window `0` — a scratch tab — closed, with its blocks.
    CloseTab(String),
}

/// Run `op` on tab `id`'s Rex scratch column in window `label` (main thread). A refusal answers at
/// once; the Rex work runs on a worker and lands through [`scratch_landed`]. Every op but a resize
/// holds the column's change ([`registry::Registry::begin_column_change`]) until it lands, which
/// disables Split and New Scratch Tab with the reason meanwhile; resizes run ungated instead, the
/// latest queued winning ([`registry::ResizeQueue`]).
#[cfg(target_os = "macos")]
fn scratch_run(app: &tauri::AppHandle, label: &str, id: &str, op: ScratchOp) -> Result<(), String> {
    use tauri::{Emitter, Manager};
    let gated = !matches!(op, ScratchOp::Resize { .. });
    let (key, dir, server, dividers, turn, save, dto) = {
        let st = app.state::<ManagerState>();
        let mut m = st.lock();
        let reg = &mut m.windows.get_mut(label).ok_or("window not found")?.registry;
        let (key, dir) = reg
            .rex_column(id)
            .ok_or("this tab has no Rex scratch column")?;
        let server = reg
            .spec_of(id)
            .and_then(|s| s.server.clone())
            .unwrap_or_default();
        let dividers = match &op {
            ScratchOp::Resize { splits, .. } => splits
                .iter()
                .map(|(split_id, _)| {
                    reg.column_divider(id, split_id)
                        .ok_or("that divider is no longer on screen")
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        let first_window = reg.column_shows_first_window(id);
        let turn = (!gated).then(|| reg.queue_column_resize(id)).flatten();
        if gated {
            reg.begin_column_change(id)?;
        }
        // The sizes, and the config entry they save to — only the first scratch tab is the
        // configured stack.
        let save = match &op {
            ScratchOp::Resize {
                sizes: Some(sizes), ..
            } if first_window => Some(config_tab(&m, label, id).map(|t| (t, sizes.clone()))),
            _ => None,
        };
        let dto = gated.then(|| m.init_dto(label)).flatten();
        (key, dir, server, dividers, turn, save, dto)
    };
    if let Some(dto) = dto {
        let _ = app.emit_to(label, "warden:refresh", dto);
        sync_agent_menu(app);
    }
    log::info!("tab {id:?}: scratch column {op:?}");
    let (held, held_label, held_id) = (app.clone(), label.to_owned(), id.to_owned());
    let (app, label, id) = (app.clone(), label.to_owned(), id.to_owned());
    std::thread::Builder::new()
        .name("warden-scratch-op".into())
        .spawn(move || {
            let Some(hub) = app.try_state::<rex::RexHub>() else {
                let main = app.clone();
                let message = format!("Rex server {server}: no connection to Rex");
                let _ = app.run_on_main_thread(move || {
                    scratch_landed(&main, &label, &id, gated, Err(message), None)
                });
                return;
            };
            // One resize of the column at a time, and only the latest queued acts.
            let _alone = turn.as_ref().map(registry::ResizeTurn::hold);
            let done = scratch_work(&hub, &key, &dir, &op, &dividers, turn.as_ref())
                .map_err(|e| rex::spawn::open_error(&server, &e));
            let superseded = turn.as_ref().is_some_and(|t| !t.is_latest());
            // A resize that took has nothing to land: the page already shows the dragged layout,
            // and the hub's view may not have it yet — landing that would jump the divider back
            // until Rex's view follows (`lifecycle::follow_column`). A superseded one leaves the
            // column and the config to the resize queued after it.
            if superseded {
                if let Err(e) = &done {
                    log::warn!("tab {id:?}: superseded scratch resize failed: {e}");
                }
                return;
            }
            if done.is_ok() && matches!(op, ScratchOp::Resize { .. }) {
                if let Some(save) = save {
                    save_column_sizes(&app, &label, &id, save);
                }
                return;
            }
            let opened = hub
                .column_session(&key)
                .and_then(|(server, sid)| hub.column(&server, &sid));
            let main = app.clone();
            let _ = app.run_on_main_thread(move || {
                scratch_landed(&main, &label, &id, gated, done, opened)
            });
        })
        .map(|_| ())
        .map_err(|e| {
            if gated {
                let st = held.state::<ManagerState>();
                let mut m = st.lock();
                if let Some(ws) = m.windows.get_mut(&held_label) {
                    ws.registry.end_column_change(&held_id);
                }
            }
            format!("couldn't start the scratch column change: {e}")
        })
}

/// Do `op` to `key`'s column session in Rex (a worker thread). A resize moves each of `dividers` —
/// `op`'s splits, looked up — in order, stopping at the first refusal, and sends nothing more once
/// a resize queued after its `turn` supersedes it.
fn scratch_work(
    hub: &rex::RexHub,
    key: &str,
    dir: &std::path::Path,
    op: &ScratchOp,
    dividers: &[rex::layout::Divider],
    turn: Option<&registry::ResizeTurn>,
) -> Result<Option<ScratchMade>, rex_client::RexError> {
    match op {
        ScratchOp::NewTab => hub
            .column_new_tab(key, dir)
            .map(|w| Some(ScratchMade::Tab(w))),
        ScratchOp::Split { block, side } => hub
            .column_split(key, block, *side, dir)
            .map(|b| Some(ScratchMade::Block(b))),
        ScratchOp::Resize { splits, .. } => {
            for ((_, ratio), d) in splits.iter().zip(dividers) {
                if turn.is_some_and(|t| !t.is_latest()) {
                    break;
                }
                hub.column_resize(key, d, *ratio)?;
            }
            Ok(None)
        }
        ScratchOp::CloseTab(w) => hub.column_close_tab(key, w).map(|()| None),
    }
}

/// Save a resized column's pane shares to its tab's config entry — `save` is that entry (or why
/// there is none) and the shares. A column whose pane count differs from the config's stack has
/// nothing to save; any other failure shows in the window's error banner.
#[cfg(target_os = "macos")]
fn save_column_sizes(
    app: &tauri::AppHandle,
    label: &str,
    id: &str,
    save: Result<((String, warden_config::Tab), Vec<f64>), String>,
) {
    let saved = save.and_then(|((title, tab), sizes)| {
        let declared = tab.split.as_ref().map_or(0, |s| s.panes.len());
        if declared != sizes.len() {
            log::debug!(
                "tab {id:?}: column of {} panes not saved: the config declares {declared}",
                sizes.len()
            );
            return Ok(());
        }
        split_write::save_pane_sizes(&title, &tab, &sizes)
    });
    if let Err(e) = saved {
        log::error!("pane sizes of tab {id:?} not saved: {e}");
        rex::lifecycle::banner(app, label, &e);
    }
}

/// What a scratch column change made in Rex: a window (scratch tab), or a block.
#[derive(Debug)]
enum ScratchMade {
    Tab(String),
    Block(String),
}

/// A scratch column change landed (main thread): the column follows Rex's session as the hub now
/// has it — a failed resize puts the dragged divider back — a new scratch tab is selected, a new
/// block takes focus once a view shows it ([`registry::Registry::focus_block`]), and the window
/// repaints. A failure shows in the window's error banner. `gated`: the op held the column's
/// change, released here.
#[cfg(target_os = "macos")]
fn scratch_landed(
    app: &tauri::AppHandle,
    label: &str,
    id: &str,
    gated: bool,
    done: Result<Option<ScratchMade>, String>,
    opened: Option<rex::column::ColumnOpened>,
) {
    use tauri::{Emitter, Manager};
    let (sizes, dto) = {
        let st = app.state::<ManagerState>();
        let mut m = st.lock();
        let Some(ws) = m.windows.get_mut(label) else {
            return;
        };
        if gated {
            ws.registry.end_column_change(id);
        }
        if let Ok(Some(ScratchMade::Block(b))) = &done {
            ws.registry.focus_block(id, b);
        }
        if let Some(opened) = opened {
            ws.registry.sync_column(id, opened);
        }
        let sizes = match &done {
            Ok(Some(ScratchMade::Tab(w))) => ws
                .registry
                .scratch_tab_of_window(id, w)
                .and_then(|t| ws.registry.select_scratch(id, t))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        (sizes, m.init_dto(label))
    };
    if let Err(e) = &done {
        log::error!("{label}: tab {id:?}: scratch column: {e}");
        rex::lifecycle::banner(app, label, e);
        // A resize's dragged layout, still held by the page, won't land.
        let _ = app.emit_to(
            label,
            "warden:scratch-hold-release",
            serde_json::json!({ "label": label, "id": id }),
        );
    }
    if let Some(dto) = dto {
        let _ = app.emit_to(label, "warden:refresh", dto);
    }
    request_rex_sizes(app, sizes);
    sync_agent_menu(app);
}

/// Sleep Rex server `server`, the user having confirmed losing `accepted` (by name). A loss not
/// among them asks again; Tab ▸ Sleep Server starts here with none.
#[cfg(target_os = "macos")]
#[tauri::command]
fn server_sleep(
    window: tauri::WebviewWindow,
    server: String,
    accepted: Vec<String>,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::sleep_server(window.app_handle(), window.label(), &server, &accepted)
}

/// Sleep Rex tab `id`'s project (its ⏻), the user having confirmed losing `accepted` (by name).
/// A loss not among them asks again; ⏻ starts here with none, through `end_session`.
#[cfg(target_os = "macos")]
#[tauri::command]
fn project_sleep(
    window: tauri::WebviewWindow,
    id: String,
    accepted: Vec<String>,
) -> Result<(), String> {
    use tauri::Manager;
    rex::lifecycle::sleep_project(window.app_handle(), window.label(), &id, &accepted)
}

/// Re-scan every window's project-tree roots and reconcile:
/// projects that appeared/vanished on disk surface as tab add/remove, without a
/// config-file edit. Diffs the freshly-expanded config against what's on screen
/// (`last_good`), so it is NOT a no-op when disk changed.
#[cfg(target_os = "macos")]
#[tauri::command]
fn rescan_root(window: tauri::WebviewWindow, state: tauri::State<ManagerState>) {
    use tauri::Manager;
    let app = window.app_handle().clone();
    // Clone the raw config under a brief lock, then run the recursive scan OFF the
    // lock so it never stalls the background probe thread; re-lock only to apply.
    // Sync `#[tauri::command]`s run on the main thread (as does hot-reload), and we
    // don't yield the main thread between the two locks, so no other writer can
    // interleave — `last_good`/`raw_config` are unchanged when we re-lock.
    let raw = state.lock().raw_config.clone();
    let fresh = manager::effective_config(&raw, app.try_state::<rex::RexHub>().as_deref());
    {
        // The window set is untouched: a rescan changes tabs, never the `[[window]]`s.
        let set = app.state::<WindowSet>();
        let mut m = state.lock();
        let recon = warden_config::reconcile(&m.last_good, &fresh);
        m.apply(&app, &recon, fresh, |t| set.label_for_title(t));
        m.record_all_sessions(&app);
    } // release the ManagerState lock before the lock-free bump
      // New discovered tabs may carry probes — fast-burst every window so their dots populate now.
    probe::bump_all(&app);
}

/// Update the calling window's pane frame from a web-coordinate rect. `pane` is the hole's
/// pane index: `None`/`Some(0)` = primary, `Some(1)` = secondary.
#[cfg(target_os = "macos")]
#[tauri::command]
fn set_hole_rect(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    rect: RectArg,
    pane: Option<usize>,
) {
    let Some(view_rect) = view_rect_of(&window, &rect) else {
        return;
    };

    // The chrome's index IS the pane (`reportRect`'s 0 = primary, 1 + i = block i, whichever
    // side the CSS draws them on); `None` is the primary. An index past the column's blocks names
    // no pane and is ignored (`from_index`, `Registry::set_pane_frame`), never an error.
    let which = crate::registry::PaneIdx::from_index(pane.unwrap_or(0));
    let mut m = state.lock();
    let rex_sizes = match m.windows.get_mut(window.label()) {
        Some(ws) => ws.registry.set_pane_frame(which, view_rect),
        None => Vec::new(),
    };
    drop(m);
    request_rex_sizes(tauri::Manager::app_handle(&window), rex_sizes);
}

/// The page's measure of each hidden tab's holes (`measureTabs` in `ui/index.html`), keyed by
/// tab id: framed while hidden (`Registry::set_tab_rects`).
#[cfg(target_os = "macos")]
#[tauri::command]
fn set_tab_rects(
    window: tauri::WebviewWindow,
    state: tauri::State<ManagerState>,
    rects: std::collections::HashMap<String, TabRectsArg>,
) {
    let rects: std::collections::HashMap<_, _> = rects
        .into_iter()
        .filter_map(|(id, r)| {
            let primary = view_rect_of(&window, &r.primary)?;
            // A block with no usable measure is skipped, its neighbours keeping their indices.
            let blocks = r.blocks.iter().map(|b| view_rect_of(&window, b)).collect();
            Some((id, (primary, blocks)))
        })
        .collect();
    let mut m = state.lock();
    let Some(ws) = m.windows.get_mut(window.label()) else {
        return;
    };
    let rex_sizes = ws.registry.set_tab_rects(&rects);
    drop(m);
    log::debug!(
        "{}: measured {} hidden tab(s); {} Rex pane(s) resized",
        window.label(),
        rects.len(),
        rex_sizes.len()
    );
    request_rex_sizes(tauri::Manager::app_handle(&window), rex_sizes);
}

#[derive(serde::Deserialize)]
struct TabRectsArg {
    primary: RectArg,
    /// One per block of the tab's column's selected scratch tab, in layout order.
    blocks: Vec<RectArg>,
}

/// A Rex pane's block takes a new grid only while warden owns it.
#[cfg(target_os = "macos")]
fn request_rex_sizes(app: &tauri::AppHandle, sizes: Vec<rex::size::PaneSize>) {
    for size in sizes {
        rex::size::request(app, rex::size::SizeAsk::IfOwner, size);
    }
}

/// A web-coordinate rect from the page as the view rect a surface frames at; `None` for a
/// non-finite rect, or a window torn down under the call.
#[cfg(target_os = "macos")]
fn view_rect_of(window: &tauri::WebviewWindow, rect: &RectArg) -> Option<surface::PixelRect> {
    // Reject non-finite values before they reach NSView or libghostty.
    if !rect.x.is_finite()
        || !rect.y.is_finite()
        || !rect.width.is_finite()
        || !rect.height.is_finite()
    {
        return None;
    }
    // Clamp to sane bounds: huge values saturate u32 in ghostty_surface_set_size.
    let x = rect.x.clamp(-100_000.0, 100_000.0);
    let y = rect.y.clamp(-100_000.0, 100_000.0);
    let width = rect.width.clamp(0.0, 100_000.0);
    let height = rect.height.clamp(0.0, 100_000.0);

    let scale = window.scale_factor().unwrap_or(1.0);
    // inner_size is in physical pixels; divide by scale to get points. A rect report
    // can race window teardown (the window's gone but a queued JS call still fires),
    // so bail rather than panic — consistent with the scale_factor fallback above.
    let size = window.inner_size().ok()?;
    let view_h = size.height as f64 / scale;
    // Snapped INWARD to the device-pixel grid: the page's flex layout puts a divider edge on a
    // fractional pixel at some window widths, and a frame rounded any other way spills over the
    // focus ring's column there — see `geometry::snap_inward`.
    Some(geometry::snap_inward(
        geometry::web_rect_to_view(
            WebRect {
                x,
                y,
                width,
                height,
            },
            view_h,
        ),
        scale,
    ))
}

/// warden's starter config, offered by the shared home surface's "Create a starter config"
/// button when no config file exists at all. Tracked, and `include_str!`'d so a missing/renamed
/// template is a build error rather than a runtime surprise.
#[cfg(target_os = "macos")]
const DEFAULT_CONFIG: &str = include_str!("default-config.toml");

/// Flips `tab_digit_keys` in the file (the Tab menu's Cycle Tabs toggle), seeding the starter
/// config first when there is none. The watcher's reload then applies it.
#[cfg(target_os = "macos")]
fn toggle_tab_digit_keys(path: &std::path::Path) -> Result<(), String> {
    warden_config::write_default_config(path, DEFAULT_CONFIG).map_err(|e| e.to_string())?;
    warden_config::toggle_tab_digit_keys(path)
        .map(|_| ())
        .map_err(|e| format!("config: {e}"))
}

/// The home surface's "Create a starter config" button. This is where config-core is called (via
/// `warden_config`'s re-export — this crate never pins config-core directly, the same
/// one-source-of-truth rule as its other re-exported house helpers) — shell-core owns the
/// surface but never touches config-core (the cores stay independent).
#[cfg(target_os = "macos")]
#[tauri::command]
fn shell_home_create_config(app: tauri::AppHandle) -> Result<(), String> {
    let path = warden_config::config_path();
    match warden_config::write_default_config(&path, DEFAULT_CONFIG) {
        // A config already existed — say so rather than report a success that didn't happen.
        Ok(false) => Err(format!(
            "{} already exists — left untouched",
            path.display()
        )),
        Ok(true) => match warden_config::load_with(&path, &warden_config::login_shell()) {
            Ok(loaded) => {
                materialize(&app, loaded.config);
                let _ = rebuild_menu(&app);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// The home surface's "Edit Config" button (shown for a config that failed to load). Reuses the
/// spine's own Edit Config action rather than a second `open` spawn.
#[cfg(target_os = "macos")]
#[tauri::command]
fn shell_home_edit_config() {
    let path = warden_config::config_path();
    shell_core::menu::handle_spine_event(shell_core::menu::ids::EDIT_CONFIG, &path);
}

/// The home surface's per-window button (shown for the `Windows` list state).
#[cfg(target_os = "macos")]
#[tauri::command]
fn shell_home_open_window(id: String, app: tauri::AppHandle) {
    open_or_focus_window(&app, &id);
}

/// Adopt a clean load with no baseline to reconcile against — launch, the home surface's starter
/// config, or the first good save after a launch without one: the `WindowSet` opens the
/// `open_on_start` windows. Expands `[[window.root]]`s into the EFFECTIVE config BEFORE taking the
/// lock — the walk is slow and must never run under the ManagerState mutex, or it stalls the
/// background probe thread for its whole duration (same discipline as `rescan_root` and the
/// hot-reload watcher). The lock is released before the set builds: it builds through
/// `WardenHost`, which takes it.
#[cfg(target_os = "macos")]
fn materialize(app: &tauri::AppHandle, config: warden_config::Config) {
    use tauri::Manager;
    let effective = manager::effective_config(&config, app.try_state::<rex::RexHub>().as_deref());
    materialize_effective(app, config, effective);
}

/// [`materialize`] from an already-scanned effective config.
#[cfg(target_os = "macos")]
fn materialize_effective(
    app: &tauri::AppHandle,
    config: warden_config::Config,
    effective: warden_config::Config,
) {
    use tauri::Manager;
    let decls = plan::decls(&effective);
    rex::unclaimed::connect(app, &effective);
    app.state::<ManagerState>().lock().adopt(config, effective);
    app.state::<WindowSet>().materialize(app, decls);
}

/// The window `⌘⇧T` would reopen right now (its label), if any.
#[cfg(target_os = "macos")]
fn reopen_target(app: &tauri::AppHandle) -> Option<String> {
    use tauri::Manager;
    let entries = app.state::<WindowSet>().entries();
    app.state::<ManagerState>().lock().reopen_target(&entries)
}

/// Open the configured window `label`, or focus it if already open (the `WindowSet` closes the
/// home surface once a real window exists), and rebuild the Window menu. The one open-by-label
/// path: the Window menu, the home surface, ⌘⇧T and `warden://open` all route here.
#[cfg(target_os = "macos")]
fn open_or_focus_window(app: &tauri::AppHandle, label: &str) {
    use tauri::Manager;
    app.state::<WindowSet>().open_or_focus(app, label);
    let _ = rebuild_menu(app);
}

/// Open (or focus) the configured window titled `title` — a pop-out's closed origin
/// (`manager::ReturnTarget::Reopen`). No-op when no window carries the title. Takes
/// `ManagerState` through `WardenHost`, so callers must hold none.
#[cfg(target_os = "macos")]
fn reopen_by_title(app: &tauri::AppHandle, title: &str) {
    use tauri::Manager;
    let set = app.state::<WindowSet>();
    let entries = set.entries();
    if let Some(home) = shell_core::windows::plan::entry_for_title(&entries, title) {
        set.open_or_focus(app, &home.id);
    }
}

/// The banner's context menu (chrome-core's `onBannerMenu`) at the chrome's `(x, y)`: Edit
/// Window… and Delete Window… for the calling window. Its items arrive as menu events.
#[cfg(target_os = "macos")]
#[tauri::command]
fn window_menu(x: f64, y: f64, window: tauri::WebviewWindow) -> Result<(), String> {
    use tauri::Manager;
    let app = window.app_handle();
    app.state::<WindowSet>()
        .banner_menu(app, window.label(), x, y)
        .map_err(|e| e.to_string())
}

/// Remove tmux's `$TMUX`/`$TMUX_PANE` from warden-app's own environment so the shells it
/// spawns never inherit them. tmux exports these into every process under a pane, and
/// warden-app is routinely launched from inside a tmux session — e.g. the very agentmux
/// session warden exists to *host*. libghostty gives each surface's shell warden-app's
/// environment verbatim, so without this scrub every tab inherits a stale `$TMUX`; tmux-based
/// tools (`amux`) then believe they're nested and refuse to build their frame, and prefix keys
/// misroute. A terminal host must present a tmux-free base environment. Must run at the very
/// top of `main()`, before any thread starts or surface spawns.
fn scrub_inherited_tmux_env() {
    for var in ["TMUX", "TMUX_PANE"] {
        std::env::remove_var(var);
    }
}

/// Make libghostty find *warden's* ghostty resources — the terminfo it needs to call itself a
/// ghostty terminal. libghostty locates them by climbing from the running executable for the
/// sentinel `Contents/Resources/terminfo/78/xterm-ghostty` (`resourcesdir.zig`); miss it and it
/// silently exports `TERM=xterm-256color` instead of `xterm-ghostty` (`termio/Exec.zig`). That
/// fallback is not cosmetic: `xterm-256color` advertises no `Sync` capability, so tmux stops
/// bracketing its redraws in DEC mode 2026 — the one signal that makes libghostty *pause*
/// rendering — so it renders half-drawn frames, and an unfocused surface (which paints its hollow
/// cursor on **every** frame, ahead of the blink gate) flickers that cursor wherever a mid-repaint
/// sample left it. Two cases, one lever — `GHOSTTY_RESOURCES_DIR`, which libghostty honours ahead
/// of the climb and from which it derives `TERMINFO` as `<parent>/terminfo`:
///
/// * **Packaged `warden.app`** — the climb succeeds on its own (the bundle ships the resources), so
///   we only *clear* an inherited `GHOSTTY_RESOURCES_DIR`. warden is routinely launched from inside
///   another ghostty/warden terminal, which exports that var; inherited, it would point warden at a
///   *different* Ghostty build's terminfo. Same reasoning as the `$TMUX` scrub: a terminal host must
///   not inherit its launcher's terminal context.
/// * **Unbundled (`cargo run` / `just run`)** — there is no bundle to climb to, so point it at the
///   vendored resources. Without this, dev runs would silently be `xterm-256color` and reproduce
///   bugs the shipped app doesn't have (and mask ones it does). Debug-only, so no build-machine path
///   is ever baked into a release binary.
///
/// Must run before libghostty is initialised — the resources dir is resolved once at app init.
fn configure_ghostty_resources() {
    // The bundle's own resources must win over whatever terminal launched us.
    std::env::remove_var("GHOSTTY_RESOURCES_DIR");

    #[cfg(debug_assertions)]
    {
        let vendor = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/resources"));
        if vendor.join("terminfo/78/xterm-ghostty").exists() {
            std::env::set_var("GHOSTTY_RESOURCES_DIR", vendor.join("ghostty"));
        } else {
            log::warn!(
                "vendored ghostty resources missing at {} — terminals will fall back to \
                 TERM=xterm-256color (no synchronized output). Run `just revendor-ghostty`.",
                vendor.display()
            );
        }
    }
}

// Sentinels bracketing the login PATH in the helper-shell output, so anything an rc file
// prints around it (banners, `nvm`/`conda` chatter) can't corrupt the readout.
const PATH_SENTINEL_START: &str = "__WARDEN_PATH_START__";
const PATH_SENTINEL_END: &str = "__WARDEN_PATH_END__";

/// Adopt the user's **login-shell PATH** so warden — and every terminal, `probe`, and `kill`
/// it spawns — can find tools the GUI launch context hides. Launched from Dock/Finder/Spotlight,
/// a `.app` inherits only the minimal launchd PATH (`/usr/bin:/bin:/usr/sbin:/sbin`); a shell,
/// probe, or kill command named without an absolute path (`fish -l` as a config override, bare
/// `tmux` in a probe) is then not found and silently fails — a `shell` override dies on spawn
/// (`exec: fish: not found`), a probe reports "no session." The built-in *default* shell
/// sidesteps this by construction (absolute `$SHELL -l`, see `warden_config::login_shell`), but
/// config-supplied commands are arbitrary and routinely bare, so they need the PATH. Rather than guess install prefixes —
/// Homebrew, nix, MacPorts and custom setups all differ — we ask the user's own login shell what
/// PATH it builds and adopt that, the approach VS Code and `exec-path-from-shell` use for the same
/// GUI-launch gap. Since surfaces/probes/kill all inherit warden-app's process env (same lever as
/// the tmux scrub), setting it once here fixes all three.
///
/// Best-effort and self-limiting: if the shell can't be run, exceeds the deadline, or yields
/// nothing parseable, PATH is left exactly as inherited — warden never ends up *worse* off than a
/// no-op. Captures the **login** environment (`-l`); a PATH set only in interactive-only rc
/// (`.zshrc` without a `.zprofile` export) isn't seen — naming such a binary by absolute path in
/// config remains the robust fallback.
fn restore_login_path() {
    use std::sync::mpsc;
    use std::time::Duration;

    // launchd populates SHELL from the user's directory record even for GUI launches; fall back
    // to the macOS default if it's somehow unset.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // Read PATH via `printenv`, NOT `echo $PATH`: the latter is shell-syntax-dependent (fish joins
    // list vars with spaces, not colons). `printenv` emits the colon-delimited PATH the login shell
    // built regardless of which shell ran it, so one snippet works for bash/zsh/fish alike.
    let snippet = format!(
        "printf %s {PATH_SENTINEL_START}; /usr/bin/printenv PATH; printf %s {PATH_SENTINEL_END}"
    );

    // Run on a side thread with a deadline so a slow/pathological login rc (conda, nvm, …) can't
    // hang warden's startup. On timeout we abandon the result; the orphan child reaps itself and
    // PATH stays as-is.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(&shell)
            .args(["-l", "-c", &snippet])
            .output();
        let _ = tx.send(out);
    });

    let Ok(Ok(out)) = rx.recv_timeout(Duration::from_secs(3)) else {
        return; // failed to spawn or timed out — keep the inherited PATH
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(path) = extract_sentinel_path(&stdout) {
        if !path.is_empty() {
            std::env::set_var("PATH", path);
        }
    }
}

/// Extract the PATH the login shell printed between the sentinels, tolerating rc-file noise
/// printed before or after it. `None` if both sentinels aren't present (e.g. the shell errored
/// or printed nothing), which the caller treats as "leave PATH untouched".
fn extract_sentinel_path(output: &str) -> Option<String> {
    let after = output.split_once(PATH_SENTINEL_START)?.1;
    let inner = after.split_once(PATH_SENTINEL_END)?.0;
    Some(inner.trim().to_string())
}

fn log_config_warnings(warnings: &[warden_config::Warning]) {
    for w in warnings {
        log::warn!("config [{}]: {}", w.window, w.message);
    }
}

/// One line for a reload's reconcile: what opens, closes and changes per window.
fn describe_reconcile(r: &warden_config::Reconciliation) -> String {
    let mut parts = Vec::new();
    if !r.open.is_empty() {
        let titles: Vec<_> = r.open.iter().map(|w| w.title.as_str()).collect();
        parts.push(format!("open {titles:?}"));
    }
    if !r.close.is_empty() {
        parts.push(format!("close {:?}", r.close));
    }
    for u in &r.update {
        let n = |v: usize, what: &str| (v > 0).then(|| format!("{v} {what}"));
        let changes: Vec<_> = [
            n(u.add_tabs.len(), "added"),
            n(u.remove_tabs.len(), "removed"),
            n(u.respawn_tabs.len(), "respawned"),
            n(u.set_meta.len(), "relabelled"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !changes.is_empty() {
            parts.push(format!("{:?}: {}", u.title, changes.join(", ")));
        }
    }
    if parts.is_empty() {
        "no window or tab changes".into()
    } else {
        parts.join("; ")
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    logging::init();
    log::info!(
        "warden {} ({}) starting",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_GIT_SHA")
    );
    // warden hosts terminals — it must not leak its own launcher's tmux membership into them
    // (breaks nested agentmux/tmux). Scrub before anything else inherits the environment.
    scrub_inherited_tmux_env();
    // A Dock/Finder/Spotlight launch gives warden only the minimal launchd PATH, so a config
    // `shell`/`probe`/`kill` named by bare command would be not-found. Adopt the login-shell PATH
    // before any surface or probe spawns and inherits this process's environment.
    restore_login_path();
    // Point libghostty at warden's own ghostty resources, so terminals get TERM=xterm-ghostty (and
    // with it synchronized output) rather than the silent xterm-256color fallback. Before ghostty
    // init: the resources dir is resolved once, at app init.
    configure_ghostty_resources();

    // libghostty must be initialised once before any app/surface is created.
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::raw::c_char;

        let args: Vec<CString> = std::env::args()
            .map(|a| CString::new(a).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let mut c_argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
        c_argv.push(std::ptr::null_mut());

        let ret = unsafe { ffi::ghostty_init(args.len(), c_argv.as_mut_ptr()) };
        if ret != 0 {
            log::warn!("ghostty_init returned {ret} (non-fatal)");
        }
    }

    // Register the shell-core plugins (geometry + updater + process) — the set every sibling app
    // installs identically. The geometry plugin persists each window's size/position (AppKit
    // points, clamped to its monitor's work area on restore) keyed by Tauri label *within a
    // per-config store* (scoped by shell-core's `geometry_filename` from the config path below) so
    // two configs sharing a window title don't share bounds; it restores on its own `on_window_ready`
    // hook, which fires for runtime-built windows too — nothing here triggers it. That covers a
    // popped-out tab's window as well, keyed by its `shell-detach:` label (deterministic per tab,
    // see `plan::detach_window_token`), so a re-popped tab reopens at the size and position it was
    // left at. Only the shared home surface is excluded, structurally inside the plugin itself, so
    // `skip_labels` carries none of warden's windows here.
    let config_path = warden_config::config_path();
    let builder = shell_core::register_plugins(tauri::Builder::default(), Some(&config_path), &[]);
    #[cfg(target_os = "macos")]
    let builder = builder.manage(EarlyOpens::default());
    builder
        // Persistent home: closing the last window shows the home surface rather than quitting
        // (`quit_on_last_close(false)` below), and closing the home surface while nothing else is
        // open quits — "closing the last surface == ⌘Q", whichever state it shows.
        .on_window_event(|window, event| {
            use tauri::Manager;
            // Sleep Server's items follow whether the focused window can ask (`sync_server_menu`).
            if matches!(event, tauri::WindowEvent::Focused(true)) {
                sync_server_menu(window.app_handle());
            }
            if window.label() == shell_core::home::HOME_LABEL
                && matches!(event, tauri::WindowEvent::Destroyed)
            {
                let app = window.app_handle();
                if let Some(set) = app.try_state::<WindowSet>() {
                    if !set.has_open() && !set.is_quitting() {
                        app.exit(0);
                    }
                }
            }
        })
        // Menu items act on the focused window. Tab nav (⌘⇧[/⌘⇧], ⌘1–⌘9) and Close Tab (⌘W)
        // route through its chrome, which owns the tab list + select()/unload. emit_to is NOT a
        // reliable per-window target here (it leaks to siblings — the same reason warden:refresh
        // carries a label, see manager.rs), so every payload carries the focused window's `label`
        // and the chrome ignores events not addressed to it. Close Window (⌘⇧W) closes it
        // directly. Unknown IDs (e.g. predefined Quit/Minimize, which self-handle) are ignored.
        .on_menu_event(|app, event| {
            use tauri::{Emitter, Manager};
            let id = event.id().as_ref();

            // The spine's file-acting ids (Edit Config, Reveal Config) need no window — let it
            // consume them first. config_path() is WARDEN_CONFIG else ~/.config/warden/config.toml.
            let cfg_path = warden_config::config_path();
            if shell_core::menu::handle_spine_event(id, &cfg_path) {
                return;
            }

            // A terminal's right-click menu: its id names the window and tab it was popped for.
            if let Some(target) = pane_menu::parse(id) {
                on_pane_menu(app, target);
                return;
            }

            // Tab ▸ Cycle Tabs: save the flip; the watcher's reload rebuilds the menu. muda already
            // flipped the tick, so a failed write rebuilds to undo it.
            if id == shell_core::menu::ids::TAB_DIGIT_CYCLE {
                if let Err(e) = toggle_tab_digit_keys(&cfg_path) {
                    // Every sidebar, and the home surface when no window is open — the watcher's
                    // failed-load path.
                    let _ = app.emit("warden:error", e.clone());
                    app.state::<WindowSet>().set_load_error(app, Some(e));
                    let _ = rebuild_menu(app);
                }
                return;
            }

            // New Window… and the banner menu's Edit / Delete Window…: shell-core's form flow.
            if app.state::<WindowSet>().handle_menu(app, id) {
                return;
            }

            // ⌘T: an agent tab, from either pane — the focused window's chrome gets the active
            // tab's id.
            if id == MENU_AGENT_NEW {
                if let Some((label, tab, ..)) = focused_agent_tab(app) {
                    let _ = app.emit_to(
                        label.as_str(),
                        id,
                        serde_json::json!({ "label": label, "tabId": tab }),
                    );
                }
                return;
            }

            // ⌥⌘S / ⌘⇧T in a tab with agent tabs act on its agent tab list: the focused window's
            // chrome gets the active tab's id. ⌥⌘S is disabled elsewhere (`sync_agent_menu`); ⌘⇧T
            // falls through to reopening a window.
            if id == MENU_AGENT_SLEEP || id == MENU_WINDOW_REOPEN_LAST {
                // ⌘⇧T reaches the list only when it has a closed agent tab; otherwise it reopens
                // a window, as everywhere else.
                let target = focused_agent_tab(app)
                    .filter(|a| id != MENU_WINDOW_REOPEN_LAST || a.3)
                    .map(|(label, tab, ..)| (label, tab));
                if let Some((label, tab)) = target {
                    let event = if id == MENU_WINDOW_REOPEN_LAST {
                        EVENT_AGENT_REOPEN
                    } else {
                        id
                    };
                    let _ = app.emit_to(
                        label.as_str(),
                        event,
                        serde_json::json!({ "label": label, "tabId": tab }),
                    );
                    return;
                }
                if id != MENU_WINDOW_REOPEN_LAST {
                    return;
                }
            }

            // ⌃⌘T: a new scratch tab in the active tab's Rex column (disabled elsewhere).
            if id == MENU_SCRATCH_NEW {
                if let Some((label, tab)) = focused_scratch_column(app) {
                    if let Err(e) = scratch_run(app, &label, &tab, ScratchOp::NewTab) {
                        rex::lifecycle::banner(app, &label, &e);
                    }
                }
                return;
            }

            // ⌘W: a selected agent tab closes, from either pane; else the project tab unloads
            // (the spine's route, below).
            if id == shell_core::menu::ids::CLOSE_TAB {
                let list = focused_tab_keys(app).and_then(|k| k.2);
                if list.as_ref().is_some_and(agent_closable) {
                    agent_key(app, EVENT_AGENT_CLOSE);
                    return;
                }
            }

            // ⌘↑ / ⌘↓ / ⌥⇧⌘↑ / ⌥⇧⌘↓ / ⌥⌘T on the agent list, ⌥⌘← / ⌥⌘→ across the panes: each
            // acts only when `sync_agent_menu` enabled it, so the step is read again here.
            if [
                MENU_AGENT_PREV,
                MENU_AGENT_NEXT,
                MENU_AGENT_PREV_ALIAS,
                MENU_AGENT_NEXT_ALIAS,
                MENU_AGENT_MOVE_UP,
                MENU_AGENT_MOVE_DOWN,
                MENU_AGENT_FORK,
                MENU_PANE_LEFT,
                MENU_PANE_RIGHT,
            ]
            .contains(&id)
            {
                agent_key(app, id);
                return;
            }

            // Window menu acts on the manager/app, not the focused window — handle it
            // before the focused-window lookup (reopen-last needs no focused window).
            if id == MENU_WINDOW_REOPEN_LAST {
                if let Some(label) = reopen_target(app) {
                    open_or_focus_window(app, &label);
                }
                return;
            }
            if let Some(win_label) = shell_core::menu::selected_window(id) {
                open_or_focus_window(app, win_label);
                return;
            }

            let Some(win) = app
                .webview_windows()
                .into_values()
                .find(|w| w.is_focused().unwrap_or(false))
            else {
                return;
            };
            let label = win.label().to_string();
            let origin_of = |l: &str| {
                let st = app.state::<ManagerState>();
                let m = st.lock();
                Some(m.windows.get(l)?.origin.as_ref()?.label.clone())
            };
            // Sleep Server ▸ <server>: the focused window confirms any loss — a pop-out's origin,
            // raised, since a pop-out's page anchors the confirm on an agent list it may not show.
            if let Some(server) = id.strip_prefix(MENU_SLEEP_SERVER) {
                let home = app.state::<ManagerState>().lock().origin_home(&label);
                let label = match home {
                    Some(manager::ReturnTarget::Open(origin)) => {
                        if let Some(w) = app.get_webview_window(&origin) {
                            let _ = w.set_focus();
                        }
                        origin
                    }
                    // The origin was closed: bring it back. Its chrome mounts fresh, with nothing
                    // listening yet for the confirm, so the command ends there.
                    Some(manager::ReturnTarget::Reopen(title)) => {
                        reopen_by_title(app, &title);
                        let _ = rebuild_menu(app);
                        return;
                    }
                    None => label,
                };
                if let Err(message) = rex::lifecycle::sleep_server(app, &label, server, &[]) {
                    let _ = app.emit_to(
                        label.as_str(),
                        "warden:error",
                        serde_json::json!({ "label": label, "message": message }),
                    );
                }
                return;
            }
            // ⌥⌘W: unload the active tab, whatever ⌘W closes in the focused pane.
            if id == MENU_TAB_CLOSE_PROJECT {
                let _ = app.emit_to(
                    label.as_str(),
                    "warden:unload-tab",
                    serde_json::json!({ "label": label }),
                );
                return;
            }
            if id == MENU_TAB_SPLIT || id == MENU_TAB_SUSPEND || id == MENU_TAB_TERMINATE {
                // ⌘D / ⌘⇧S / ⌘⇧K act on the active tab. The chrome owns "which tab is active", so
                // it drives the command on this event. Label-stamped + forMe()-filtered like every
                // other per-window emit (emit_to leaks to sibling webviews).
                let _ = app.emit_to(label.as_str(), id, serde_json::json!({ "label": label }));
                return;
            }
            if let Some(routed) = shell_core::menu::route(id, &label, origin_of) {
                on_spine_action(app, routed);
            }
        })
        .invoke_handler(tauri::generate_handler![
            set_hole_rect,
            set_tab_rects,
            scratch_select,
            scratch_new,
            scratch_close_tab,
            scratch_resize,
            save_split_size,
            init_tabs,
            activate_tab,
            unload_tab,
            split_pane,
            focus_pane,
            focus_sidebar,
            pop_out_tab,
            raise_popped_window,
            pop_in_tab,
            end_session,
            start_session,
            rescan_root,
            shell_home_create_config,
            shell_home_edit_config,
            shell_home_open_window,
            window_menu,
            pane_menu,
            probe_now,
            agents_init,
            agent_seen,
            agent_new,
            agent_select,
            agent_sleep,
            agent_wake,
            agent_fork,
            agent_close,
            agent_reopen,
            agent_rename,
            agent_group,
            agent_reorder,
            agent_fold,
            agent_fold_family,
            agent_clear_closed,
            server_sleep,
            project_sleep
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;

                let handle = app.handle().clone();
                let mut mgr = WindowManager::new();
                let config_dir = handle.path().app_config_dir().ok();
                let scoped = |stem: &str| {
                    config_dir.as_ref().map(|dir| {
                        dir.join(shell_core::config_scoped_filename(
                            stem,
                            &warden_config::config_path(),
                        ))
                    })
                };
                if let Some(path) = scoped(session::STEM) {
                    mgr.session = session::SessionStore::load(path);
                }
                // The Rex hub connects nothing until a tab with `server` needs a session.
                let rex_info = rex_client::ClientInfo {
                    kind: "app",
                    name: "warden".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                };
                let hub = match scoped(rex::store::STEM) {
                    Some(path) => rex::RexHub::new(path, rex_info),
                    None => rex::RexHub::in_memory(rex_info),
                };
                app.manage(rex::size::Sizer::start(hub.clone()));
                app.manage(hub);
                // What warden keeps about agent tabs, read and written by agent panes' opens.
                app.manage(std::sync::Mutex::new(match scoped(rex::agents::STEM) {
                    Some(path) => rex::agents::AgentStore::load(path),
                    None => rex::agents::AgentStore::default(),
                }));
                app.manage(std::sync::Mutex::new(rex::attention::Attention::default()));
                // The window set builds through `WardenHost`, which locks `ManagerState`, so both
                // are managed before anything opens. Closing the last window shows the home surface
                // instead of quitting (see the Builder's `on_window_event`).
                app.manage(ManagerState(std::sync::Mutex::new(mgr)));
                app.manage(AgentMenu::default());
                app.manage(ServerMenu::default());
                app.manage(
                    WindowSet::new("warden", warden_config::config_path(), windows::WardenHost)
                        .quit_on_last_close(false),
                );
                // Load config; a config that parses (even with zero `[[window]]` entries) always
                // materializes — the set opens nothing for an empty window list and shows the home
                // surface's window list (empty or not). A missing/invalid config instead records
                // the error and the set shows the home surface directly: `NoConfig` when the file
                // doesn't exist, `Broken` when it does but didn't load. Recovery from either happens
                // in the watcher: the first valid load with no baseline materializes.
                // Read the `notify_debug` toggle from the loaded config (default false) before the
                // config is consumed by materialize — it gates notify.rs's diagnostic trace.
                let mut notify_debug = false;
                match warden_config::load_with(
                    &warden_config::config_path(),
                    &warden_config::login_shell(),
                ) {
                    Ok(loaded) => {
                        log_config_warnings(&loaded.warnings);
                        log::info!("config loaded: {} window(s)", loaded.config.windows.len());
                        notify_debug = loaded.config.notify_debug;
                        surface::set_trusted_link_schemes(
                            loaded.config.trusted_link_schemes.clone(),
                        );
                        materialize(&handle, loaded.config);
                    }
                    Err(e) => {
                        log::warn!("config didn't load: {e}");
                        handle
                            .state::<WindowSet>()
                            .set_load_error(&handle, Some(e.to_string()))
                    }
                }

                // Route terminal attention signals (bell / OSC 9/777 desktop notification) from
                // surfaces to their tabs (badge + macOS banner). Installs the surface-event sink;
                // needs ManagerState already managed (above) since the handler resolves surfaces
                // through it. `notify_debug` (config, default false) gates the diagnostic trace.
                notify::init(handle.clone(), notify_debug);

                // Rex tabs' presence dots follow the hub's session lists (not the probe
                // scheduler, which skips them).
                rex::spawn::watch_presence(handle.clone());
                // Agent tab lists follow Rex's windows and statuses and the relays' resume
                // reports.
                rex::lifecycle::watch_agents(handle.clone());
                rex::lifecycle::watch_reports(handle.clone());

                // Background presence-probe scheduler — the single probe driver. Per-window
                // fast-until-stable bursts on key moments (tab activate, window focus/open, session
                // start/kill, hot-reload/rescan), decaying to the slow floor (`probe_interval`) or
                // Idle when it's 0. Triggers call `probe::bump`; nothing else spawns probe passes.
                {
                    let st = handle.state::<ManagerState>();
                    let interval = st.lock().probe_interval.clone();
                    let app_sched = handle.clone();
                    std::thread::spawn(move || probe::run_scheduler(app_sched, interval));
                }
                // No launch-time probe pass here: each window's chrome calls the
                // `probe_now` command once its `warden:session-state` listener is
                // registered (see init() in index.html), which populates the dots
                // reliably without racing the listener — covering `probe_interval = 0`
                // and background windows that never emit a launch `Focused`.

                // macOS menu. Windows are built at runtime with no NSMenu, so without this the
                // standard shortcuts are dead and there's nowhere to surface tab navigation.
                // Predefined items (Minimize/Quit) self-handle; custom items fire the Builder's
                // on_menu_event. Tab chords ⌘⇧[/⌘⇧] (prev/next) and the digit chords (⌘1–9
                // jump, or ⌘1/⌘2 cycle + ⌘3–9 jump under `tab_digit_keys = "cycle"`) are
                // macOS-standard and checked app-wide before any view, so they never collide
                // with the terminal. ⌘W unloads the active *tab* and ⌘⇧W closes the *window* — the Safari/
                // Chrome convention (close-tab vs close-window), NOT the predefined ⌘W=close-window.
                // The ⌘1/⌘2 chords depend on the config's `tab_digit_keys` mode
                // (read from last_good, set by the load above; default Jump when the
                // load failed and last_good is still the empty default). build_app_menu
                // rebuilds wholesale, so a hot-reload that flips the mode just calls it
                // again (see the watcher).
                rebuild_menu(app.handle())?;

                // Hot-reload: watch the config file; on each event reload + diff
                // against last_good, hand the windows to the `WindowSet` and apply
                // the kept windows' tab ops. The notify callback runs on a background thread, but
                // every Tauri/AppKit/registry touch is main-thread only — hop via
                // run_on_main_thread before doing any of it.
                let cfg_path = warden_config::config_path();
                // Watcher::with_default requires the config's parent dir to already exist.
                if let Some(parent) = cfg_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let wh = app.handle().clone();
                // The formatter's copy of the path (cfg_path is moved into the watcher).
                let fmt_path = cfg_path.clone();
                // Inject the login shell so hot-reload uses the same default as the initial load.
                let watcher = warden_config::Watcher::with_default(
                    cfg_path,
                    warden_config::login_shell(),
                    move |res| {
                        let wh = wh.clone();
                        let fmt_path = fmt_path.clone();
                        let _ = wh.clone().run_on_main_thread(move || {
                            use tauri::{Emitter, Manager};
                            match res {
                                Ok(loaded) => {
                                    log_config_warnings(&loaded.warnings);
                                    surface::set_trusted_link_schemes(
                                        loaded.config.trusted_link_schemes.clone(),
                                    );
                                    // Expand `[[window.root]]`s into the EFFECTIVE config
                                    // (recursive git-project scan) BEFORE taking the lock —
                                    // the walk is slow and must never run under the
                                    // ManagerState mutex, or it stalls the background probe
                                    // thread for its whole duration (mirrors probe.rs's
                                    // snapshot-then-release discipline). We're on the main
                                    // thread here, so no other writer interleaves. A config
                                    // that parses but declares zero `[[window]]` entries is
                                    // NOT special-cased here: the window set on an empty
                                    // windows list simply opens nothing / closes everything,
                                    // and shows the home surface's (now-empty) window list —
                                    // not an error state.
                                    let new_eff = manager::effective_config(
                                        &loaded.config,
                                        wh.try_state::<rex::RexHub>().as_deref(),
                                    );
                                    let st = wh.state::<ManagerState>();
                                    let set = wh.state::<WindowSet>();
                                    let mut m = st.lock();
                                    // The app menu is global, not part of window reconcile;
                                    // rebuilt below from current state.
                                    // Density + sidebar_drag are global too — a change to either
                                    // alone yields an empty reconcile (no per-window op), so nudge
                                    // every chrome below. `open_tabs_section` needs no entry here:
                                    // it cascades to the window, so resolve collapses even a
                                    // global-only edit into a per-window diff reconcile emits.
                                    let old_density = m.last_good.density;
                                    let new_density = loaded.config.density;
                                    let old_drag = m.last_good.sidebar_drag;
                                    let new_drag = loaded.config.sidebar_drag;
                                    // Recover (materialize, fresh-launch semantics that respect
                                    // open_on_start) ONLY when there is no baseline to reconcile
                                    // against — an empty `last_good` means we never had a valid
                                    // config. Reconciling from an empty baseline would emit Open for
                                    // EVERY window, since `reconcile` deliberately ignores
                                    // `open_on_start`, wrongly opening `open_on_start = false` ones.
                                    //
                                    // Do NOT also recover on "the home surface is showing an error":
                                    // the home-surface state can *become* Broken (close every
                                    // window, then save a half-written config — the Err branch below
                                    // records the error regardless of live windows), and
                                    // recovering on the next good save would
                                    // re-materialize every window the user deliberately closed. An
                                    // empty `last_good` already covers every genuine no-baseline case
                                    // (invalid/missing config at launch, or a config later saved with
                                    // zero windows); a real baseline always has a non-empty
                                    // `last_good` (even a config with configured-but-closed windows),
                                    // so it correctly reconciles instead — unchanged config ⇒ windows
                                    // stay closed; an added window ⇒ opens.
                                    let recover = m.last_good.windows.is_empty();
                                    if recover {
                                        // Recovery: no reconcile baseline. Materialize from the
                                        // already-scanned effective config rather than reconciling
                                        // against an empty last_good.
                                        log::info!(
                                            "config reloaded: materializing {} window(s)",
                                            new_eff.windows.len()
                                        );
                                        drop(m);
                                        materialize_effective(&wh, loaded.config.clone(), new_eff);
                                    } else {
                                        // Reconcile against the EFFECTIVE (root-expanded) configs
                                        // so a project appearing/vanishing on disk since last load
                                        // surfaces as a tab add/remove. Re-scans on every reload.
                                        let recon =
                                            warden_config::reconcile(&m.last_good, &new_eff);
                                        log::info!(
                                            "config reloaded: {}",
                                            describe_reconcile(&recon)
                                        );
                                        let decls = plan::decls(&new_eff);
                                        // Advances the reconcile baseline — this branch is
                                        // only reached on a valid load.
                                        m.apply(&wh, &recon, new_eff, |t| set.label_for_title(t));
                                        m.adopt_raw(loaded.config.clone());
                                        rex::unclaimed::connect(&wh, &m.last_good);
                                        // A density/sidebar_drag flip alone produces no per-window
                                        // op, so apply() emitted nothing; re-push every window's
                                        // snapshot (now carrying the new globals) so each restyles.
                                        if old_density != new_density || old_drag != new_drag {
                                            m.refresh_all_chrome(&wh);
                                        }
                                        drop(m);
                                        // Open added windows, close removed ones, recolour kept
                                        // ones (a window the user closed stays closed); clears
                                        // the home surface's error. Lock released: the set builds
                                        // through `WardenHost`, which takes it.
                                        set.apply(&wh, decls);
                                        st.lock().record_all_sessions(&wh);
                                    }
                                    // Opt-in tidy: rewrite the file formatted. Diff-guarded in
                                    // format_file, so warden's own write doesn't loop the watcher.
                                    // Only runs on a clean parse (this branch).
                                    if loaded.config.format_on_save {
                                        let _ = warden_config::format_file(&fmt_path);
                                    }
                                    // Rebuild the global app menu: the digit-keys mode and/or the
                                    // window set may have changed.
                                    let _ = rebuild_menu(&wh);
                                    // Clear any stale error banner.
                                    let _ = wh.emit("warden:error-clear", ());
                                    // Refresh the session dots now that cadence/config may have changed
                                    // (lock already released, so the bump can lock freely).
                                    probe::bump_all(&wh);
                                }
                                Err(e) => {
                                    // Keep last_good; record the error with the window set, whose
                                    // home surface shows it only when nothing is open. The banner is
                                    // a separate concern (a parse error mid-edit while windows are
                                    // open needs its own sidebar notice).
                                    let msg = e.to_string();
                                    log::warn!(
                                        "config reload failed, keeping the last good one: {msg}"
                                    );
                                    let set = wh.state::<WindowSet>();
                                    set.set_load_error(&wh, Some(msg.clone()));
                                    if set.has_open() {
                                        let _ = wh.emit("warden:error", msg);
                                    }
                                }
                            }
                        });
                    },
                );
                // Keep the watcher alive for the app's lifetime. Log a failure so a
                // dead watcher (no hot-reload) is distinguishable from a working one.
                match watcher {
                    Ok(w) => {
                        app.manage(WatcherState(w));
                    }
                    Err(e) => {
                        log::error!("failed to start config watcher (no hot-reload): {e}");
                    }
                }

                let held = app
                    .state::<EarlyOpens>()
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .unwrap_or_default();
                for req in held {
                    open_url(app.handle(), req);
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building warden")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { .. } => {
                if let Some(set) = tauri::Manager::try_state::<WindowSet>(app) {
                    set.mark_quitting();
                }
            }
            // A `warden://` URL (CFBundleURLTypes in Info.plist). On a cold launch it arrives
            // before `setup`, which opens it once the window set exists (`EarlyOpens`).
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Opened { urls } => {
                for req in urls.iter().filter_map(|u| open_request(u, "warden")) {
                    open_or_hold(app, req);
                }
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_session_items_disable_with_their_reason_in_a_popout() {
        assert_eq!(
            end_session_item(SUSPEND_TITLE, None),
            (true, "Suspend Session".to_owned())
        );
        assert_eq!(
            end_session_item(TERMINATE_TITLE, Some(END_SESSION_POPOUT)),
            (
                false,
                "Terminate Session… (dock the tab to end its session)".to_owned()
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn toggling_the_digit_mode_seeds_a_missing_config_first() {
        use warden_config::TabDigitKeys;
        let dir = std::env::temp_dir().join(format!("warden-digit-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        toggle_tab_digit_keys(&path).unwrap();
        let src = std::fs::read_to_string(&path).unwrap();
        // The starter's documentation survives; only the value changed.
        assert!(
            src.starts_with(DEFAULT_CONFIG.lines().next().unwrap()),
            "{src}"
        );
        let shell = warden_config::login_shell();
        let loaded = warden_config::load_with(&path, &shell).unwrap();
        assert_eq!(loaded.config.tab_digit_keys, TabDigitKeys::Cycle);
        toggle_tab_digit_keys(&path).unwrap();
        let loaded = warden_config::load_with(&path, &shell).unwrap();
        assert_eq!(loaded.config.tab_digit_keys, TabDigitKeys::Jump);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warden_open_urls_name_a_window_through_the_shell_core_helper() {
        let t = |s: &str| open_request(&tauri::Url::parse(s).unwrap(), "warden")?.window;
        assert_eq!(t("warden://open?window=work").as_deref(), Some("work"));
        assert_eq!(
            t("warden://open?window=my%20work").as_deref(),
            Some("my work")
        );
        assert_eq!(t("warden://open?x=1"), None);
        assert_eq!(t("warden://close?window=work"), None);
        assert_eq!(t("curator://open?window=work"), None);
    }

    #[test]
    fn tab_rects_carry_one_rect_per_block_in_order() {
        let r = |x: f64| serde_json::json!({ "x": x, "y": 0.0, "width": 10.0, "height": 5.0 });
        let arg: TabRectsArg = serde_json::from_value(serde_json::json!({
            "primary": r(0.0),
            "blocks": [r(1.0), r(2.0)],
        }))
        .unwrap();
        assert_eq!(arg.primary.x, 0.0);
        assert_eq!(
            arg.blocks.iter().map(|b| b.x).collect::<Vec<_>>(),
            [1.0, 2.0]
        );
        let unsplit: TabRectsArg =
            serde_json::from_value(serde_json::json!({ "primary": r(0.0), "blocks": [] })).unwrap();
        assert!(unsplit.blocks.is_empty());
        // The page always sends `blocks`; a payload without it is malformed, not unsplit.
        assert!(
            serde_json::from_value::<TabRectsArg>(serde_json::json!({ "primary": r(0.0) }))
                .is_err()
        );
    }

    #[test]
    fn scrub_removes_inherited_tmux_vars() {
        // Simulate warden-app launched from inside a tmux pane (the agentmux session it hosts).
        std::env::set_var("TMUX", "/tmp/tmux-501/agentmux-term,2109,29");
        std::env::set_var("TMUX_PANE", "%43");
        scrub_inherited_tmux_env();
        assert!(std::env::var_os("TMUX").is_none(), "TMUX must be scrubbed");
        assert!(
            std::env::var_os("TMUX_PANE").is_none(),
            "TMUX_PANE must be scrubbed"
        );
    }

    #[test]
    fn extract_sentinel_path_pulls_path_from_noisy_output() {
        // A login rc that prints a banner before and after the PATH readout must not corrupt it.
        let out = format!(
            "Welcome back!\n{PATH_SENTINEL_START}/opt/homebrew/bin:/usr/bin:/bin\n{PATH_SENTINEL_END}\nnvm: loaded\n"
        );
        assert_eq!(
            extract_sentinel_path(&out).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin:/bin")
        );
    }

    #[test]
    fn extract_sentinel_path_none_without_both_sentinels() {
        // Shell errored / printed nothing usable → leave PATH untouched.
        assert_eq!(extract_sentinel_path("command not found"), None);
        assert_eq!(
            extract_sentinel_path(&format!("{PATH_SENTINEL_START}/usr/bin")),
            None
        );
    }
}
