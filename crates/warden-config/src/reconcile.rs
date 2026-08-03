use crate::model::{Config, Tab, Window};
use crate::Colour;

/// The in-place, non-respawn metadata of a kept tab — fields a consumer can apply to a *live*
/// tab without killing its PTY: its display `title`, `group` (sidebar sectioning), and the
/// externally-run `probe`/`kill` commands. Never the terminal itself. Carried by
/// `WindowUpdate.set_meta` when any of these changed for a kept tab (keyed by `Tab::key`).
#[derive(Debug, Clone, PartialEq)]
pub struct TabMeta {
    pub group: Option<String>,
    pub probe: Option<String>,
    pub kill: Option<String>,
    /// New display title for the kept tab. Applied as a live relabel — the chrome
    /// re-renders the row without respawning its terminal. Always carried (it's the
    /// tab's current title), so a metadata update always relabels to the current title.
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reconciliation {
    pub open: Vec<Window>,
    pub close: Vec<String>,
    pub update: Vec<WindowUpdate>,
}

/// Describes in-place mutations needed for a window that stays open across a
/// config reload.
///
/// **What IS detected** (any of these triggers an emit):
/// - `colour`: the window accent colour changed.
/// - `add_tabs` / `remove_tabs`: tabs were added or removed, matched by
///   `Tab::key` (the `id`-else-normalized-`dir` for a curated tab; the absolute
///   project path for a tab discovered by a `[[window.root]]` scan).
/// - `tab_order`: the order of kept tabs changed; on an emitted update
///   `tab_order` always carries the full new ordered key list so the consumer
///   can reorder the live tab strip without killing sessions.
/// - `set_meta`: a kept tab's in-place metadata (the display `title`, `group`,
///   `probe`, or `kill`) changed. Each entry is `(key, TabMeta)` carrying the new
///   values; the consumer applies them WITHOUT respawning (presentation +
///   externally-run commands).
/// - `respawn_tabs`: a kept tab's terminal spec (`dir`, `shell`, `cmd`, or
///   `load_on_open`) changed. The consumer tears down and respawns that tab in
///   place, carrying its new full `Tab`.
///
/// **What is NOT detected:**
/// - A kept window's `width` or `height` change. Window size is owned by
///   shell-core's geometry plugin after first launch and is a first-run default only;
///   subsequent changes to those fields in the config have no effect on a live
///   window.
///
/// **Window renames are destructive.** A rename appears as `close(old) +
/// open(new)`, killing and recreating that window's PTYs (including
/// `load_on_open` tabs). There is no concept of a live retitle.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowUpdate {
    pub title: String,
    pub colour: Option<Colour>,
    pub add_tabs: Vec<Tab>,
    pub remove_tabs: Vec<String>,
    pub tab_order: Vec<String>,
    /// In-place metadata changes for kept tabs (keyed by `Tab::key`): `title`,
    /// `group`, `probe`, or `kill` differ. The consumer applies them WITHOUT
    /// respawning — presentation + externally-run commands, never the PTY. Empty
    /// when no kept tab's metadata changed.
    pub set_meta: Vec<(String, TabMeta)>,
    /// Kept tabs (same `key`) whose terminal spec changed — `dir`, `shell`, `startup`
    /// (`cmd`), or `load_on_open` differ. The consumer tears down and respawns each in
    /// place (a terminal cannot migrate its cwd/session). Carries the new full `Tab`.
    /// Empty when no kept tab's terminal spec changed. (A kept tab whose *dir* changed
    /// with NO explicit `id` changes key instead and rides `add_tabs`/`remove_tabs`.)
    pub respawn_tabs: Vec<Tab>,
}

fn find<'a>(windows: &'a [Window], name: &str) -> Option<&'a Window> {
    windows.iter().find(|p| p.title == name)
}

/// Diff two configs and return the set of operations needed to bring a running
/// session from `old` to `new`.
///
/// **What IS detected:**
/// - Windows opened/closed, matched by `title`.
/// - For a kept window: colour change, tab add/remove (by `Tab::key` —
///   `id`-else-normalized-`dir` for a curated tab, absolute project path for a discovered one), tab
///   reorder (via `tab_order`), in-place metadata changes (the display `title`,
///   `group`, `probe`, `kill`) via `set_meta`, and a kept tab's terminal-spec
///   change (`dir`/`shell`/`cmd`/`load_on_open`) via `respawn_tabs` (respawn in
///   place).
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
                // Kept-tab in-place diffs. Two independent signals per kept tab:
                //  - metadata (title/group/probe/kill) → live apply, no respawn.
                //  - terminal spec (dir/shell/startup/load_on_open) → respawn in place.
                let mut set_meta: Vec<(String, TabMeta)> = Vec::new();
                let mut respawn_tabs: Vec<Tab> = Vec::new();
                for nt in &np.tabs {
                    if let Some(ot) = op.tabs.iter().find(|ot| ot.key == nt.key) {
                        if ot.title != nt.title
                            || ot.group != nt.group
                            || ot.probe != nt.probe
                            || ot.kill != nt.kill
                        {
                            set_meta.push((
                                nt.key.clone(),
                                TabMeta {
                                    group: nt.group.clone(),
                                    probe: nt.probe.clone(),
                                    kill: nt.kill.clone(),
                                    title: nt.title.clone(),
                                },
                            ));
                        }
                        if ot.dir != nt.dir
                            || ot.shell != nt.shell
                            || ot.startup != nt.startup
                            || ot.load_on_open != nt.load_on_open
                        {
                            respawn_tabs.push(nt.clone());
                        }
                    }
                }
                if colour.is_some()
                    || !add_tabs.is_empty()
                    || !remove_tabs.is_empty()
                    || order_changed
                    || !set_meta.is_empty()
                    || !respawn_tabs.is_empty()
                {
                    update.push(WindowUpdate {
                        title: np.title.clone(),
                        colour,
                        add_tabs,
                        remove_tabs,
                        tab_order,
                        set_meta,
                        respawn_tabs,
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
                    title: "api".to_string(),
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
                    title: "api".to_string(),
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
                    title: "api".to_string(),
                }
            )]
        );
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
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
                    title: "api".to_string(),
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
}
