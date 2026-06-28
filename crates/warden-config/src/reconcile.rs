use std::path::PathBuf;

use crate::colour::Colour;
use crate::model::{Config, Tab, Window};

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
/// - `icon`: the window icon changed. Outer `Some` = changed; the inner
///   `Option<PathBuf>` is the new value, which may be `Some(path)` or `None`
///   to clear the icon.
/// - `add_tabs` / `remove_tabs`: tabs were added or removed, matched by
///   `Tab::key` (the resolved title).
/// - `tab_order`: the order of kept tabs changed; on an emitted update
///   `tab_order` always carries the full new ordered key list so the consumer
///   can reorder the live tab strip without killing sessions.
/// - `set_groups`: a kept tab's `group` (its `[[window.group]]` membership)
///   changed — including a pure group rename, where order and keys are
///   identical. Each entry is `(key, new_group)`; the consumer re-sections the
///   sidebar **without** respawning (grouping is presentation only). Empty when
///   no kept tab's group changed.
///
/// **What is NOT detected:**
/// - In-place edits to a kept tab whose title is unchanged. Changing a tab's
///   `dir`, `cmd`, or `keep_alive` while keeping its `title` the same produces
///   no op — the tab appears identical to the reconciler. The consumer must
///   close and reopen the tab to pick up such field-level edits. (`group` is the
///   deliberate exception — it IS detected, via `set_groups`, because it's
///   presentational and must not cost the tab its PTY.)
///
/// **Window renames are destructive.** A rename appears as `close(old) +
/// open(new)`, killing and recreating that window's PTYs (including
/// `keep_alive` tabs). There is no concept of a live retitle.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowUpdate {
    pub name: String,
    pub colour: Option<Colour>,
    pub icon: Option<Option<PathBuf>>,
    pub add_tabs: Vec<Tab>,
    pub remove_tabs: Vec<String>,
    pub tab_order: Vec<String>,
    pub set_groups: Vec<(String, Option<String>)>,
}

fn find<'a>(windows: &'a [Window], name: &str) -> Option<&'a Window> {
    windows.iter().find(|p| p.name == name)
}

/// Diff two configs and return the set of operations needed to bring a running
/// session from `old` to `new`.
///
/// **What IS detected:**
/// - Windows opened/closed, matched by `name`.
/// - For a kept window: colour change, icon change, tab add/remove (by
///   `Tab::key` = resolved title), and tab reorder (via `tab_order`).
///
/// **What is NOT detected:**
/// - In-place edits to a kept tab whose title is unchanged. If a tab's `dir`,
///   `cmd`, or `keep_alive` changes but its `title` stays the same, no update
///   is emitted — the tab appears identical to the reconciler. The consumer
///   must close and reopen the tab to pick up such field-level edits.
///
/// **Window renames are destructive** — a rename appears as `close(old) +
/// open(new)`, killing and recreating that window's PTYs (including
/// `keep_alive` tabs).
pub fn reconcile(old: &Config, new: &Config) -> Reconciliation {
    let mut open = Vec::new();
    let mut close = Vec::new();
    let mut update = Vec::new();

    // closed: in old, not in new
    for op in &old.windows {
        if find(&new.windows, &op.name).is_none() {
            close.push(op.name.clone());
        }
    }

    for np in &new.windows {
        match find(&old.windows, &np.name) {
            None => open.push(np.clone()),
            Some(op) => {
                let colour = (op.colour != np.colour).then_some(np.colour);
                let icon = (op.icon != np.icon).then(|| np.icon.clone());
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
                // Group reassignment of kept tabs (matched by key). Carries the new
                // group so the consumer re-sections without respawning — a pure group
                // rename, where keys/order are unchanged, is detected here too.
                let set_groups: Vec<(String, Option<String>)> = np
                    .tabs
                    .iter()
                    .filter_map(|nt| {
                        op.tabs
                            .iter()
                            .find(|ot| ot.key == nt.key)
                            .filter(|ot| ot.group != nt.group)
                            .map(|_| (nt.key.clone(), nt.group.clone()))
                    })
                    .collect();
                if colour.is_some()
                    || icon.is_some()
                    || !add_tabs.is_empty()
                    || !remove_tabs.is_empty()
                    || order_changed
                    || !set_groups.is_empty()
                {
                    update.push(WindowUpdate {
                        name: np.name.clone(),
                        colour,
                        icon,
                        add_tabs,
                        remove_tabs,
                        tab_order,
                        set_groups,
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
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##;

    #[test]
    fn added_window_goes_to_open() {
        let old = cfg("");
        let new = cfg(BASE);
        let r = reconcile(&old, &new);
        assert_eq!(r.open.len(), 1);
        assert_eq!(r.open[0].name, "work");
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
name = "work"
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
            vec!["ops"]
        );
        assert_eq!(u.remove_tabs, vec!["locus".to_string()]);
        assert_eq!(u.colour, None);
    }

    #[test]
    fn reorder_only_emits_update_with_new_order() {
        let old = cfg(r##"
[[window]]
name = "work"
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
name = "work"
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
        assert_eq!(u.tab_order, vec!["beta".to_string(), "alpha".to_string()]);
        assert!(u.add_tabs.is_empty(), "no tabs added");
        assert!(u.remove_tabs.is_empty(), "no tabs removed");
        assert_eq!(u.colour, None, "no colour change");
        assert_eq!(u.icon, None, "no icon change");
    }

    #[test]
    fn icon_change_emits_update() {
        let old = cfg(r##"
[[window]]
name = "work"
colour = "#0f8a8a"
icon = "/tmp/old.png"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##);
        let new = cfg(r##"
[[window]]
name = "work"
colour = "#0f8a8a"
icon = "/tmp/new.png"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##);
        let r = reconcile(&old, &new);
        assert_eq!(r.update.len(), 1, "expected a WindowUpdate for icon change");
        let u = &r.update[0];
        assert_eq!(
            u.icon,
            Some(Some(PathBuf::from("/tmp/new.png"))),
            "icon should carry the new value"
        );
        assert!(u.add_tabs.is_empty() && u.remove_tabs.is_empty());
        assert_eq!(u.colour, None);
    }

    /// Regression test locking the documented limitation: a kept tab whose
    /// `title` (and therefore `key`) is unchanged but whose `dir` differs is
    /// invisible to the reconciler. No update is emitted; the consumer must
    /// close and reopen the tab to pick up such field-level edits.
    #[test]
    fn in_place_tab_field_edit_is_not_detected() {
        let old = cfg(r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##);
        let new = cfg(r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "locus"
  dir = "/tmp/locus-new"
"##);
        let r = reconcile(&old, &new);
        assert!(
            r.update.is_empty(),
            "in-place tab field edits (dir change with same title) must not emit an update"
        );
    }

    #[test]
    fn pure_group_rename_emits_update_with_set_groups_only() {
        // Same tab, same key, same order — only the group name changed. Must emit an
        // update carrying set_groups (and nothing else) so the sidebar re-sections
        // without respawning the PTY.
        let old = cfg(r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.group]]
  name = "old-name"
    [[window.group.tab]]
    title = "api"
    dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
name = "work"
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
            u.set_groups,
            vec![("api".to_string(), Some("new-name".to_string()))]
        );
        assert!(u.add_tabs.is_empty() && u.remove_tabs.is_empty());
        assert_eq!(u.colour, None);
        assert_eq!(u.icon, None);
    }

    #[test]
    fn moving_loose_tab_into_group_sets_its_group() {
        // A loose tab gains a group. set_groups carries the new membership (here the
        // flat order is unchanged, so the change is detected purely via set_groups).
        let old = cfg(r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "api"
  dir = "/tmp/api"
"##);
        let new = cfg(r##"
[[window]]
name = "work"
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
            r.update[0].set_groups,
            vec![("api".to_string(), Some("backend".to_string()))]
        );
    }

    #[test]
    fn unchanged_groups_emit_no_update() {
        let same = r##"
[[window]]
name = "work"
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
