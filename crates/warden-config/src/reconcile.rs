use crate::model::{Config, Split, SplitSide, StackPane, Tab, Window};
use crate::Colour;
use std::path::Path;

/// The in-place, non-respawn metadata of a kept tab — what a consumer can apply to a *live*
/// tab without killing its PTY; never the terminal itself. Carried by `WindowUpdate.set_meta`
/// (keyed by `Tab::key`); which changes put a tab there is listed once, on [`reconcile`].
#[derive(Debug, Clone, PartialEq)]
pub struct TabMeta {
    pub group: Option<String>,
    pub probe: Option<String>,
    pub kill: Option<String>,
    pub suspend: Option<String>,
    /// The program a woken agent's resume command runs (`Tab::resume`).
    pub resume: Option<String>,
    /// New display title for the kept tab. Applied as a live relabel — the chrome
    /// re-renders the row without respawning its terminal. Always carried (it's the
    /// tab's current title), so a metadata update always relabels to the current title.
    pub title: String,
    /// The kept tab's current declared split. Carried on every metadata update (like
    /// `title`); a `side`/`size`-only change is a live re-layout, so it rides here. The split's
    /// PRESENCE rides `split_tabs`/`unsplit_tabs`, and (on a plain tab) its `startup` rides
    /// `respawn_tabs` — the second pane has to be spawned or closed for either.
    pub split: Option<Split>,
    /// The kept tab's pinned project colour (`Tab::colour`), carried like `title`.
    pub colour: Option<Colour>,
}

/// What a tab's terminals run — the fields a running PTY can't take on in place. The one home for
/// what a change to them costs is [`TerminalSpec::change_to`]: [`reconcile`] and the app's
/// popped-out-tab return path both ask it, so a docked and a detached tab decide alike.
/// `split_startup` is `None` for no declared split, `Some(None)` for a split running a bare shell
/// (or, on a Rex tab, any split: its panes ride `set_meta`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalSpec<'a> {
    pub dir: &'a Path,
    pub shell: &'a str,
    pub startup: Option<&'a str>,
    pub split_startup: Option<Option<&'a str>>,
    /// The Rex server hosting the tab (`None` = plain tab); a change moves the panes to another server.
    pub server: Option<&'a str>,
}

impl<'a> TerminalSpec<'a> {
    pub fn new(
        dir: &'a Path,
        shell: &'a str,
        startup: Option<&'a str>,
        split: Option<&'a Split>,
        server: Option<&'a str>,
    ) -> Self {
        Self {
            dir,
            shell,
            startup,
            // A Rex tab's panes are all sessions created at spawn: its stack is `set_meta`
            // territory, so only the split's presence rides the spec there.
            split_startup: split.map(|s| match server {
                Some(_) => None,
                None => s.scratch_startup(),
            }),
            server,
        }
    }
}

/// How a kept tab's terminals must follow its [`TerminalSpec`] from `old` to `new`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpecChange {
    Same,
    /// Only the split appeared: the second pane comes up beside the running primary.
    Split,
    /// Only the split went: the second pane closes, the primary keeps running.
    Unsplit,
    /// Anything else a running PTY can't take on: tear down and respawn both panes.
    Respawn,
}

impl TerminalSpec<'_> {
    /// What it takes to bring terminals running `self` to `new`. A split's presence alone never
    /// costs the primary its PTY; its content (`split_startup`) on both sides does.
    pub fn change_to(&self, new: &TerminalSpec) -> SpecChange {
        if (self.dir, self.shell, self.startup, self.server)
            != (new.dir, new.shell, new.startup, new.server)
        {
            return SpecChange::Respawn;
        }
        match (self.split_startup, new.split_startup) {
            (None, Some(_)) => SpecChange::Split,
            (Some(_), None) => SpecChange::Unsplit,
            (a, b) if a != b => SpecChange::Respawn,
            _ => SpecChange::Same,
        }
    }
}

impl Tab {
    pub fn terminal_spec(&self) -> TerminalSpec<'_> {
        TerminalSpec::new(
            &self.dir,
            &self.shell,
            self.startup.as_deref(),
            self.split.as_ref(),
            self.server.as_deref(),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reconciliation {
    pub open: Vec<Window>,
    pub close: Vec<String>,
    pub update: Vec<WindowUpdate>,
}

/// Describes in-place mutations needed for a window that stays open across a
/// config reload. What each field carries, and what is not detected: [`reconcile`].
#[derive(Debug, Clone, PartialEq)]
pub struct WindowUpdate {
    pub title: String,
    pub colour: Option<Colour>,
    /// The window's `open_tabs_section` flipped — `Some(new)`, else `None`. Chrome-only
    /// (the sidebar grows or drops its pinned "Open" section), so the consumer just
    /// restyles: no surface is touched. A GLOBAL edit reaches a window through here too,
    /// since the cascade is already collapsed into `Window::open_tabs_section`.
    pub open_tabs_section: Option<bool>,
    /// The window's `background_tabs_section` flipped — chrome-only, exactly like
    /// `open_tabs_section`.
    pub background_tabs_section: Option<bool>,
    pub add_tabs: Vec<Tab>,
    pub remove_tabs: Vec<String>,
    pub tab_order: Vec<String>,
    /// Kept tabs to update WITHOUT respawning (keyed by `Tab::key`).
    pub set_meta: Vec<(String, TabMeta)>,
    /// Kept tabs to tear down and respawn in place, each carrying its new full `Tab`.
    pub respawn_tabs: Vec<Tab>,
    /// Kept tabs that gained a split, each carrying its new full `Tab`: the second pane is
    /// declared and comes up beside a live primary, which keeps running.
    pub split_tabs: Vec<Tab>,
    /// Kept tabs (by `Tab::key`) whose split went: the second pane closes, the primary stays.
    pub unsplit_tabs: Vec<String>,
    /// Kept tabs (by `Tab::key`) whose `load_on_open` turned on: spawn now if cold. A live
    /// tab is left running.
    pub spawn_tabs: Vec<String>,
}

fn find<'a>(windows: &'a [Window], name: &str) -> Option<&'a Window> {
    windows.iter().find(|p| p.title == name)
}

/// The dir of the `[[window.root]]` tab `t` was discovered under, if any (its `group` names the
/// root). A discovered tab's sidebar nesting derives from this dir, which the tab itself doesn't
/// carry — so a moved root re-derives its kept tabs through `set_meta`.
fn root_dir<'a>(w: &'a Window, t: &Tab) -> Option<&'a Path> {
    let group = t.group.as_deref()?;
    w.roots
        .iter()
        .find(|r| r.name == group)
        .map(|r| r.dir.as_path())
}

/// Diff two configs and return the set of operations needed to bring a running
/// session from `old` to `new`.
///
/// **What IS detected:**
/// - Windows opened/closed, matched by `title`.
/// - For a kept window (a `WindowUpdate`, emitted when any of these is non-empty):
///   - `colour`, `open_tabs_section` and `background_tabs_section` (chrome-only) changes.
///   - The window's tree sections (`Window::tree_names`) changing — chrome-only, and an update
///     even when the added or removed root discovers no tab; the consumer re-sends them.
///   - `add_tabs`/`remove_tabs`, matched by `Tab::key` — `id`-else-normalized-`dir` for a
///     curated tab, the absolute project path for a discovered one. A kept tab whose `dir`
///     changed with no explicit `id` changes key, so it is a remove + add.
///   - `tab_order`: when kept tabs reorder; an emitted update always carries the full new key list.
///   - `set_meta` — a live update, no respawn: a kept tab's display `title`, `group`, `probe`,
///     `kill`, `suspend`, `resume`, `colour`, split `side`/`size` or, on a Rex tab, its pane stack (a live re-layout), or the dir of the
///     `[[window.root]]` it was discovered under (its sidebar nesting derives from it).
///   - `respawn_tabs` — torn down and respawned in place, since a terminal can't migrate its
///     cwd/session: a kept tab's [`TerminalSpec`] ([`SpecChange::Respawn`]) — including a
///     plain tab's split `cmd`/last pane while the split stays.
///   - `split_tabs` / `unsplit_tabs` — a kept tab's split appeared / went with nothing else in
///     its `TerminalSpec` changing: the second pane opens / closes and the primary keeps its PTY.
///   - `spawn_tabs` — a kept tab's `load_on_open` turned on. `load_on_open` says only when a
///     tab comes up, so it never respawns one: turning it on spawns a cold tab, and turning
///     it off leaves a live one running until the next launch.
///
/// **What is NOT detected:**
/// - A kept window's `width` or `height` change. Window size is owned by
///   shell-core's geometry plugin after first launch and is a first-run default only;
///   subsequent changes to those fields in the config are not applied to a live
///   window.
/// - A window's `open_on_start` flag. It is a launch-only materialization gate
///   (consulted solely by the app's `materialize`), NOT a live-session concern,
///   so `reconcile` never consults it: changing it never produces an op, and a
///   window newly *added* to the config (or renamed, which is close+open) is
///   emitted as `open` and opens immediately **regardless of `open_on_start`** —
///   the flag only suppresses auto-open at process launch. This is deliberate
///   (launch-only scope); do not "fix" reconcile to gate `open` on the flag.
///
/// **Window renames are destructive** — a rename appears as `close(old) +
/// open(new)`, killing and recreating that window's PTYs (including
/// `load_on_open` tabs).
pub fn reconcile(old: &Config, new: &Config) -> Reconciliation {
    let mut open = Vec::new();
    let mut close = Vec::new();
    let mut update = Vec::new();

    // closed: in old, not in new
    for op in &old.windows {
        if find(&new.windows, &op.title).is_none() {
            close.push(op.title.clone());
        }
    }

    for np in &new.windows {
        match find(&old.windows, &np.title) {
            None => open.push(np.clone()),
            Some(op) => {
                let colour = (op.colour != np.colour).then_some(np.colour);
                let open_tabs_section =
                    (op.open_tabs_section != np.open_tabs_section).then_some(np.open_tabs_section);
                let background_tabs_section = (op.background_tabs_section
                    != np.background_tabs_section)
                    .then_some(np.background_tabs_section);
                let old_keys: Vec<&str> = op.tabs.iter().map(|t| t.key.as_str()).collect();
                let new_keys: Vec<&str> = np.tabs.iter().map(|t| t.key.as_str()).collect();
                let add_tabs: Vec<Tab> = np
                    .tabs
                    .iter()
                    .filter(|t| !old_keys.contains(&t.key.as_str()))
                    .cloned()
                    .collect();
                let remove_tabs: Vec<String> = op
                    .tabs
                    .iter()
                    .filter(|t| !new_keys.contains(&t.key.as_str()))
                    .map(|t| t.key.clone())
                    .collect();
                // order_changed: the kept tabs appear in a different sequence
                let kept_old: Vec<&str> = old_keys
                    .iter()
                    .copied()
                    .filter(|k| new_keys.contains(k))
                    .collect();
                let kept_new: Vec<&str> = new_keys
                    .iter()
                    .copied()
                    .filter(|k| old_keys.contains(k))
                    .collect();
                let order_changed = kept_old != kept_new;
                let tab_order: Vec<String> = np.tabs.iter().map(|t| t.key.clone()).collect();
                // Kept-tab in-place diffs: two independent signals per kept tab, `set_meta` and
                // `respawn_tabs` (the fields behind each are listed on this fn's docblock).
                let mut set_meta: Vec<(String, TabMeta)> = Vec::new();
                let mut respawn_tabs: Vec<Tab> = Vec::new();
                let mut split_tabs: Vec<Tab> = Vec::new();
                let mut unsplit_tabs: Vec<String> = Vec::new();
                let mut spawn_tabs: Vec<String> = Vec::new();
                for nt in &np.tabs {
                    if let Some(ot) = op.tabs.iter().find(|ot| ot.key == nt.key) {
                        fn layout(t: &Tab) -> Option<(SplitSide, f64, &[StackPane])> {
                            let s = t.split.as_ref()?;
                            let panes = if t.server.is_some() {
                                &s.panes[..]
                            } else {
                                &[]
                            };
                            Some((s.side, s.size, panes))
                        }
                        if ot.title != nt.title
                            || ot.group != nt.group
                            || ot.probe != nt.probe
                            || ot.kill != nt.kill
                            || ot.suspend != nt.suspend
                            || ot.resume != nt.resume
                            || ot.colour != nt.colour
                            || (ot.split.is_some()
                                && nt.split.is_some()
                                && layout(ot) != layout(nt))
                            || root_dir(op, ot) != root_dir(np, nt)
                        {
                            set_meta.push((
                                nt.key.clone(),
                                TabMeta {
                                    group: nt.group.clone(),
                                    probe: nt.probe.clone(),
                                    kill: nt.kill.clone(),
                                    suspend: nt.suspend.clone(),
                                    resume: nt.resume.clone(),
                                    title: nt.title.clone(),
                                    split: nt.split.clone(),
                                    colour: nt.colour,
                                },
                            ));
                        }
                        let change = ot.terminal_spec().change_to(&nt.terminal_spec());
                        match change {
                            // A respawn already honours the new load_on_open.
                            SpecChange::Respawn => respawn_tabs.push(nt.clone()),
                            SpecChange::Split => split_tabs.push(nt.clone()),
                            SpecChange::Unsplit => unsplit_tabs.push(nt.key.clone()),
                            SpecChange::Same => {}
                        }
                        if change != SpecChange::Respawn && !ot.load_on_open && nt.load_on_open {
                            spawn_tabs.push(nt.key.clone());
                        }
                    }
                }
                if colour.is_some()
                    || op.tree_names() != np.tree_names()
                    || open_tabs_section.is_some()
                    || background_tabs_section.is_some()
                    || !add_tabs.is_empty()
                    || !remove_tabs.is_empty()
                    || order_changed
                    || !set_meta.is_empty()
                    || !respawn_tabs.is_empty()
                    || !split_tabs.is_empty()
                    || !unsplit_tabs.is_empty()
                    || !spawn_tabs.is_empty()
                {
                    update.push(WindowUpdate {
                        title: np.title.clone(),
                        colour,
                        open_tabs_section,
                        background_tabs_section,
                        add_tabs,
                        remove_tabs,
                        tab_order,
                        set_meta,
                        respawn_tabs,
                        split_tabs,
                        unsplit_tabs,
                        spawn_tabs,
                    });
                }
            }
        }
    }

    Reconciliation {
        open,
        close,
        update,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::parse;
    use crate::resolve::resolve;

    fn cfg(s: &str) -> Config {
        resolve(parse(s).unwrap()).unwrap().0
    }

    const BASE: &str = r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
"##;

    #[test]
    fn added_window_goes_to_open() {
        let old = cfg("");
        let new = cfg(BASE);
        let r = reconcile(&old, &new);
        assert_eq!(r.open.len(), 1);
        assert_eq!(r.open[0].title, "work");
        assert!(r.close.is_empty() && r.update.is_empty());
    }

    #[test]
    fn removed_window_goes_to_close() {
        let r = reconcile(&cfg(BASE), &cfg(""));
        assert_eq!(r.close, vec!["work".to_string()]);
        assert!(r.open.is_empty() && r.update.is_empty());
    }

    #[test]
    fn identical_config_is_noop() {
        let r = reconcile(&cfg(BASE), &cfg(BASE));
        assert!(r.open.is_empty() && r.close.is_empty() && r.update.is_empty());
    }

    #[test]
    fn colour_change_emits_update_with_colour() {
        let new = cfg(&BASE.replace("#0f8a8a", "#112233"));
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(
            r.update[0].colour,
            Some(Colour {
                r: 0x11,
                g: 0x22,
                b: 0x33
            })
        );
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
    }

    #[test]
    fn load_on_open_never_respawns_and_turning_it_on_spawns() {
        let on = cfg(&BASE.replace(
            "dir = \"/tmp/alpha\"",
            "dir = \"/tmp/alpha\"\n  load_on_open = true",
        ));
        let r = reconcile(&cfg(BASE), &on);
        assert_eq!(r.update.len(), 1);
        assert!(r.update[0].respawn_tabs.is_empty());
        assert_eq!(
            r.update[0].spawn_tabs,
            vec![on.windows[0].tabs[0].key.clone()]
        );
        // Off again: nothing to do — a live tab keeps running until the next launch.
        let r = reconcile(&on, &cfg(BASE));
        assert!(r.update.is_empty());
    }

    #[test]
    fn open_tabs_section_change_emits_update_and_touches_no_tab() {
        let new = cfg(&BASE.replace(
            "colour = \"#0f8a8a\"",
            "colour = \"#0f8a8a\"\nopen_tabs_section = true",
        ));
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].open_tabs_section, Some(true));
        // Chrome-only: nothing about the window's terminals changed.
        assert!(r.update[0].colour.is_none());
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
        assert!(r.update[0].set_meta.is_empty() && r.update[0].respawn_tabs.is_empty());
    }

    #[test]
    fn background_tabs_section_change_emits_update_and_touches_no_tab() {
        let new = cfg(&BASE.replace(
            "colour = \"#0f8a8a\"",
            "colour = \"#0f8a8a\"\nbackground_tabs_section = false",
        ));
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].background_tabs_section, Some(false));
        assert!(r.update[0].open_tabs_section.is_none());
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
        assert!(r.update[0].set_meta.is_empty() && r.update[0].respawn_tabs.is_empty());
    }

    #[test]
    fn global_open_tabs_section_flip_reaches_an_inheriting_window() {
        // The cascade is collapsed by resolve, so a GLOBAL-only edit is an ordinary
        // per-window diff here — no separate global-compare path is needed.
        let old = cfg(BASE);
        let new = cfg(&format!("open_tabs_section = true\n{BASE}"));
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].open_tabs_section, Some(true));
    }

    #[test]
    fn added_and_removed_tabs_within_kept_window() {
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "ops"
  dir = "/tmp/ops"
"##);
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert_eq!(
            u.add_tabs
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["/tmp/ops"]
        );
        assert_eq!(u.remove_tabs, vec!["/tmp/alpha".to_string()]);
        assert_eq!(u.colour, None);
    }

    #[test]
    fn reorder_only_emits_update_with_new_order() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  [[window.tab]]
  title = "beta"
  dir = "/tmp/beta"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "beta"
  dir = "/tmp/beta"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(
            r.update.len(),
            1,
            "expected exactly one WindowUpdate for a tab reorder"
        );
        let u = &r.update[0];
        assert_eq!(
            u.tab_order,
            vec!["/tmp/beta".to_string(), "/tmp/alpha".to_string()]
        );
        assert!(u.add_tabs.is_empty(), "no tabs added");
        assert!(u.remove_tabs.is_empty(), "no tabs removed");
        assert_eq!(u.colour, None, "no colour change");
    }

    #[test]
    fn window_rename_is_destructive_close_plus_open() {
        // Windows are diffed by name, so a rename is not an update — it's close(old)
        // + open(new), which destroys and recreates that window's terminals (incl.
        // load_on_open ones). Pins the documented destructive-rename behaviour.
        let new = cfg(&BASE.replace(r#"title = "work""#, r#"title = "play""#));
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.close, vec!["work".to_string()]);
        assert_eq!(r.open.len(), 1);
        assert_eq!(r.open[0].title, "play");
        assert!(r.update.is_empty(), "a rename is never an in-place update");
    }

    /// A kept tab (identity pinned by an explicit `id`) whose `dir` changes emits a
    /// `respawn_tabs` entry — the tab restarts in the new dir without add/remove.
    #[test]
    fn dir_change_on_id_pinned_tab_emits_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  id = "a"
  title = "alpha"
  dir = "/tmp/alpha"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  id = "a"
  title = "alpha"
  dir = "/tmp/alpha-new"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert_eq!(
            u.respawn_tabs
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(
            u.respawn_tabs[0].dir,
            std::path::PathBuf::from("/tmp/alpha-new")
        );
        assert!(u.add_tabs.is_empty() && u.remove_tabs.is_empty());
    }

    #[test]
    fn pure_group_rename_emits_update_with_set_meta_only() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.group]]
  name = "old-name"
    [[window.group.tab]]
    title = "api"
    dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.group]]
  name = "new-name"
    [[window.group.tab]]
    title = "api"
    dir = "/tmp/api"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1, "a pure group rename must emit an update");
        let u = &r.update[0];
        assert_eq!(
            u.set_meta,
            vec![(
                "/tmp/api".to_string(),
                TabMeta {
                    group: Some("new-name".to_string()),
                    probe: None,
                    kill: None,
                    suspend: None,
                    resume: None,
                    title: "api".to_string(),
                    split: None,
                    colour: None,
                }
            )]
        );
        assert!(u.add_tabs.is_empty() && u.remove_tabs.is_empty());
        assert_eq!(u.colour, None);
    }

    #[test]
    fn moving_loose_tab_into_group_sets_its_meta() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "api"
  dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.group]]
  name = "backend"
    [[window.group.tab]]
    title = "api"
    dir = "/tmp/api"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(
            r.update[0].set_meta,
            vec![(
                "/tmp/api".to_string(),
                TabMeta {
                    group: Some("backend".to_string()),
                    probe: None,
                    kill: None,
                    suspend: None,
                    resume: None,
                    title: "api".to_string(),
                    split: None,
                    colour: None,
                }
            )]
        );
    }

    #[test]
    fn probe_change_on_kept_tab_emits_set_meta() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "api"
  dir = "/tmp/api"
  probe = "probe-old"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "api"
  dir = "/tmp/api"
  probe = "probe-new"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(
            r.update.len(),
            1,
            "a probe change on a kept tab must emit an update"
        );
        assert_eq!(
            r.update[0].set_meta,
            vec![(
                "/tmp/api".to_string(),
                TabMeta {
                    group: None,
                    probe: Some("probe-new".to_string()),
                    kill: None,
                    suspend: None,
                    resume: None,
                    title: "api".to_string(),
                    split: None,
                    colour: None,
                }
            )]
        );
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
    }

    #[test]
    fn colour_change_on_kept_tab_emits_set_meta_not_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp/api"
  colour = "#0f8a8a"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].set_meta.len(), 1);
        assert_eq!(
            r.update[0].set_meta[0].1.colour,
            Some(Colour {
                r: 0x0f,
                g: 0x8a,
                b: 0x8a
            })
        );
        assert!(r.update[0].respawn_tabs.is_empty());
    }

    #[test]
    fn suspend_change_on_kept_tab_emits_set_meta_not_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp/api"
  suspend = "suspend-new"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(
            r.update[0].set_meta[0].1.suspend.as_deref(),
            Some("suspend-new")
        );
        assert!(r.update[0].respawn_tabs.is_empty());
    }

    #[test]
    fn resume_change_on_kept_tab_emits_set_meta_not_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp/api"
  resume = "agent-wrapper"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(
            r.update[0].set_meta[0].1.resume.as_deref(),
            Some("agent-wrapper")
        );
        assert!(r.update[0].respawn_tabs.is_empty());
    }

    #[test]
    fn kill_change_on_kept_tab_emits_set_meta() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "api"
  dir = "/tmp/api"
  kill = "kill-old"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "api"
  dir = "/tmp/api"
  kill = "kill-new"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(
            r.update.len(),
            1,
            "a kill change on a kept tab must emit an update"
        );
        assert_eq!(
            r.update[0].set_meta,
            vec![(
                "/tmp/api".to_string(),
                TabMeta {
                    group: None,
                    probe: None,
                    kill: Some("kill-new".to_string()),
                    suspend: None,
                    resume: None,
                    title: "api".to_string(),
                    split: None,
                    colour: None,
                }
            )]
        );
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
    }

    #[test]
    fn title_change_relabels_via_set_meta_no_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/a"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "renamed"
  dir = "/tmp/a"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert_eq!(u.set_meta.len(), 1);
        assert_eq!(u.set_meta[0].0, "/tmp/a");
        assert_eq!(u.set_meta[0].1.title, "renamed");
        assert!(u.respawn_tabs.is_empty() && u.add_tabs.is_empty() && u.remove_tabs.is_empty());
    }

    #[test]
    fn title_and_dir_change_together_emit_both_set_meta_and_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  id = "a"
  title = "alpha"
  dir = "/tmp/a"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  id = "a"
  title = "renamed"
  dir = "/tmp/a-new"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        // title change → set_meta (relabel), dir change (id-pinned, same key) → respawn
        assert_eq!(u.set_meta.len(), 1);
        assert_eq!(u.set_meta[0].0, "a");
        assert_eq!(u.set_meta[0].1.title, "renamed");
        assert_eq!(
            u.respawn_tabs
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(
            u.respawn_tabs[0].dir,
            std::path::PathBuf::from("/tmp/a-new")
        );
        assert!(u.add_tabs.is_empty() && u.remove_tabs.is_empty());
    }

    #[test]
    fn dir_change_without_id_is_add_remove_not_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/a"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/b"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert_eq!(
            u.add_tabs
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["/tmp/b"]
        );
        assert_eq!(u.remove_tabs, vec!["/tmp/a".to_string()]);
        assert!(u.respawn_tabs.is_empty());
    }

    #[test]
    fn cmd_change_on_kept_dir_tab_emits_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/a"
  cmd = "old"
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/a"
  cmd = "new"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(
            r.update[0]
                .respawn_tabs
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            vec!["/tmp/a"]
        );
        assert!(r.update[0].set_meta.is_empty());
    }

    #[test]
    fn unchanged_groups_emit_no_update() {
        let same = r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.group]]
  name = "g"
    [[window.group.tab]]
    title = "api"
    dir = "/tmp/api"
"##;
        let r = reconcile(&cfg(same), &cfg(same));
        assert!(r.update.is_empty(), "identical grouped config is a no-op");
    }

    /// A kept tab whose split `size` changes is a live relabel (set_meta), never a respawn.
    #[test]
    fn split_size_change_rides_set_meta_not_respawn() {
        let old = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/alpha"
  [window.tab.split]
  size = 0.3
"##);
        let new = cfg(r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/alpha"
  [window.tab.split]
  size = 0.4
  side = "left"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert!(u.respawn_tabs.is_empty());
        assert_eq!(u.set_meta.len(), 1);
        let s = u.set_meta[0].1.split.as_ref().unwrap();
        assert_eq!((s.side, s.size), (crate::SplitSide::Left, 0.4));
    }

    const WITH_SPLIT_CMD: &str = r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  [window.tab.split]
  cmd = "scratch"
"##;

    /// Adding a split brings the second pane up beside the running primary — never a respawn.
    #[test]
    fn adding_split_is_split_tabs_not_respawn() {
        let with = cfg(WITH_SPLIT_CMD);
        let r = reconcile(&cfg(BASE), &with);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert!(u.respawn_tabs.is_empty());
        assert!(u.set_meta.is_empty() && u.unsplit_tabs.is_empty());
        assert_eq!(u.split_tabs, vec![with.windows[0].tabs[0].clone()]);
        // Identical splits → no update at all.
        assert!(reconcile(&with, &with).update.is_empty());
    }

    #[test]
    fn removing_split_is_unsplit_tabs() {
        let r = reconcile(&cfg(WITH_SPLIT_CMD), &cfg(BASE));
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert!(u.respawn_tabs.is_empty() && u.split_tabs.is_empty());
        assert_eq!(u.unsplit_tabs, vec!["/tmp/alpha".to_string()]);
    }

    /// A plain tab's split content is its second pane's startup: changing it respawns.
    #[test]
    fn plain_tab_split_cmd_change_still_respawns() {
        let other = cfg(&WITH_SPLIT_CMD.replace("\"scratch\"", "\"other\""));
        let r = reconcile(&cfg(WITH_SPLIT_CMD), &other);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert_eq!(u.respawn_tabs.len(), 1);
        assert!(u.split_tabs.is_empty() && u.unsplit_tabs.is_empty());
        assert!(u.set_meta.is_empty());
    }

    #[test]
    fn rex_tab_split_add_is_split_tabs() {
        let unsplit = REX_STACK
            .split("  [[window.tab.split.pane]]")
            .next()
            .unwrap();
        let with = cfg(REX_STACK);
        let r = reconcile(&cfg(unsplit), &with);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert!(u.respawn_tabs.is_empty());
        assert_eq!(u.split_tabs, vec![with.windows[0].tabs[0].clone()]);
    }

    /// A split added alongside another terminal-spec change rides the respawn, which brings
    /// both panes up from the new tab.
    #[test]
    fn split_added_with_a_dir_change_respawns_once() {
        let mut moved = WITH_SPLIT_CMD.replace("dir = \"/tmp/alpha\"", "dir = \"/tmp/beta\"");
        moved = moved.replace("title = \"alpha\"", "title = \"alpha\"\n  id = \"a\"");
        let base = BASE.replace("title = \"alpha\"", "title = \"alpha\"\n  id = \"a\"");
        let r = reconcile(&cfg(&base), &cfg(&moved));
        let u = &r.update[0];
        assert_eq!(u.respawn_tabs.len(), 1);
        assert!(u.split_tabs.is_empty() && u.unsplit_tabs.is_empty());
    }

    /// A kept discovered tab under a root whose `dir` moved re-derives its sidebar nesting, so it
    /// rides `set_meta` though none of the tab's own fields changed.
    #[test]
    fn a_moved_root_relabels_its_kept_tabs_via_set_meta() {
        let with_root = |root: &str| {
            let mut c = cfg(&format!(
                r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/a/b/alpha"
  [[window.root]]
  name = "projects"
  dir = "{root}"
"##
            ));
            // A discovered tab, as the app's scanner synthesizes it: grouped under its root.
            c.windows[0].tabs[0].group = Some("projects".into());
            c
        };
        let r = reconcile(&with_root("/tmp/a"), &with_root("/tmp/a/b"));
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].set_meta.len(), 1);
        assert!(r.update[0].respawn_tabs.is_empty());
        // Same root dir → no update.
        assert!(reconcile(&with_root("/tmp/a"), &with_root("/tmp/a"))
            .update
            .is_empty());
    }

    /// A root that discovers nothing changes no tab, but the sidebar still grows or drops its
    /// (empty) tree section, so adding, renaming or removing one is an update.
    #[test]
    fn an_empty_root_added_renamed_or_removed_emits_an_update() {
        let with_roots = |roots: &str| cfg(&format!("{BASE}{roots}"));
        let none = with_roots("");
        let one = with_roots("  [[window.root]]\n  name = \"empty\"\n  dir = \"/tmp/nothing\"\n");
        let renamed =
            with_roots("  [[window.root]]\n  name = \"other\"\n  dir = \"/tmp/nothing\"\n");
        for (old, new) in [(&none, &one), (&one, &renamed), (&one, &none)] {
            let r = reconcile(old, new);
            assert_eq!(r.update.len(), 1);
            assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
        }
        assert!(reconcile(&one, &one).update.is_empty());
    }

    #[test]
    fn server_change_respawns() {
        let mk = |server: &str| {
            cfg(&format!(
                r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  server = "{server}"
"##
            ))
        };
        let r = reconcile(&mk("local"), &mk("remote-host"));
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].respawn_tabs.len(), 1);
        assert!(reconcile(&mk("local"), &mk("local")).update.is_empty());
    }

    const REX_STACK: &str = r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "alpha"
  dir = "/tmp/alpha"
  server = "local"
  [[window.tab.split.pane]]
  cmd = "a"
  [[window.tab.split.pane]]
  cmd = "b"
"##;

    #[test]
    fn pane_change_on_rex_tab_is_set_meta() {
        let old = cfg(REX_STACK);
        let new = cfg(&REX_STACK.replace("cmd = \"b\"", "cmd = \"c\"\n  size = 0.3"));
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].set_meta.len(), 1);
        assert!(r.update[0].respawn_tabs.is_empty());
    }

    #[test]
    fn pane_change_on_plain_tab_respawns() {
        let plain = REX_STACK.replace("  server = \"local\"\n", "");
        // A plain tab keeps only its last pane, so only the last pane's cmd matters.
        let old = cfg(&plain);
        let new = cfg(&plain.replace("cmd = \"b\"", "cmd = \"c\""));
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].respawn_tabs.len(), 1);
    }
}
