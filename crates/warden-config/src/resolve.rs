use crate::colour::{Colour, ColourError};
use crate::model::{Config, Tab, Warning, Window};
use crate::raw::{RawConfig, RawWindow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// The shell spawned in a tab when no `shell` is set at any level. Each tab runs the
/// cascaded `shell`; a tab's cascaded `cmd`, if any, is auto-run *inside* it.
pub const DEFAULT_SHELL: &str = "fish -l";

/// Resolve a cascading setting — the nearest *explicitly set* level wins (tab > window >
/// global). An explicitly-empty value (`""`) still counts as "set", so it resets to unset
/// rather than inheriting: that's how `cmd = ""` on a tab opts out of an inherited command.
fn cascade<'a>(
    tab: Option<&'a str>,
    window: Option<&'a str>,
    global: Option<&'a str>,
) -> Option<&'a str> {
    tab.or(window).or(global).filter(|s| !s.trim().is_empty())
}

#[derive(Debug, Error, PartialEq)]
pub enum ResolveError {
    #[error("duplicate window name: {0:?}")]
    DuplicateWindow(String),
    #[error("window {window:?} has duplicate tab title: {title:?}")]
    DuplicateTab { window: String, title: String },
    #[error("window {window:?} has a tab with an empty dir")]
    EmptyDir { window: String },
    #[error("window {window:?} has invalid colour")]
    BadColour {
        window: String,
        #[source]
        source: ColourError,
    },
    #[error("window at index {index} has an empty name")]
    EmptyWindowName { index: usize },
    #[error("window {window:?} has a tab with an empty explicit title")]
    EmptyTabTitle { window: String },
    #[error("window {window:?} has a group with an empty name")]
    EmptyGroupName { window: String },
    #[error("window {window:?} has duplicate group: {group:?}")]
    DuplicateGroup { window: String, group: String },
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

pub fn resolve(raw: RawConfig) -> Result<(Config, Vec<Warning>), ResolveError> {
    let global_shell = raw.shell.as_deref();
    let global_cmd = raw.cmd.as_deref();
    let mut warnings = Vec::new();
    let mut windows = Vec::with_capacity(raw.windows.len());
    let mut seen_windows = HashSet::new();

    for (index, rp) in raw.windows.iter().enumerate() {
        if rp.name.trim().is_empty() {
            return Err(ResolveError::EmptyWindowName { index });
        }
        if !seen_windows.insert(rp.name.clone()) {
            return Err(ResolveError::DuplicateWindow(rp.name.clone()));
        }
        windows.push(resolve_window(rp, global_shell, global_cmd, &mut warnings)?);
    }
    Ok((Config { windows }, warnings))
}

fn resolve_window(
    rp: &RawWindow,
    global_shell: Option<&str>,
    global_cmd: Option<&str>,
    warnings: &mut Vec<Warning>,
) -> Result<Window, ResolveError> {
    let colour = Colour::parse(&rp.colour).map_err(|source| ResolveError::BadColour {
        window: rp.name.clone(),
        source,
    })?;
    let icon = rp.icon.as_deref().map(expand_tilde);

    // Flatten loose tabs + each group's tabs into one ordered list: loose first
    // (ungrouped, headerless), then each `[[window.group]]` in file order, tabs
    // within a group keeping file order. Groups add no cascade level — they're
    // presentation only — so every tab resolves identically (tab → window → global)
    // and just carries its group name. Title uniqueness is window-wide (shared
    // `seen_titles` across loose + grouped tabs), matching `Tab::key`.
    let total: usize = rp.tabs.len() + rp.groups.iter().map(|g| g.tabs.len()).sum::<usize>();
    let mut tabs = Vec::with_capacity(total);
    let mut seen_titles = HashSet::new();

    for rt in &rp.tabs {
        tabs.push(resolve_tab(
            rt,
            None,
            rp,
            global_shell,
            global_cmd,
            &mut seen_titles,
            warnings,
        )?);
    }

    let mut seen_groups = HashSet::new();
    for g in &rp.groups {
        if g.name.trim().is_empty() {
            return Err(ResolveError::EmptyGroupName {
                window: rp.name.clone(),
            });
        }
        if !seen_groups.insert(g.name.clone()) {
            return Err(ResolveError::DuplicateGroup {
                window: rp.name.clone(),
                group: g.name.clone(),
            });
        }
        for rt in &g.tabs {
            tabs.push(resolve_tab(
                rt,
                Some(g.name.clone()),
                rp,
                global_shell,
                global_cmd,
                &mut seen_titles,
                warnings,
            )?);
        }
    }

    Ok(Window {
        name: rp.name.clone(),
        colour,
        icon,
        tabs,
    })
}

/// Resolve one raw tab into a `Tab`, tagged with `group` (`None` = loose/ungrouped).
/// Shared by the loose-tab and grouped-tab passes so both validate and cascade
/// identically; `seen_titles` is threaded in to enforce window-wide title uniqueness.
#[allow(clippy::too_many_arguments)]
fn resolve_tab(
    rt: &crate::raw::RawTab,
    group: Option<String>,
    rp: &RawWindow,
    global_shell: Option<&str>,
    global_cmd: Option<&str>,
    seen_titles: &mut HashSet<String>,
    warnings: &mut Vec<Warning>,
) -> Result<Tab, ResolveError> {
    if rt.dir.trim().is_empty() {
        return Err(ResolveError::EmptyDir {
            window: rp.name.clone(),
        });
    }
    let dir = expand_tilde(&rt.dir);
    if let Some(ref t) = rt.title {
        if t.trim().is_empty() {
            return Err(ResolveError::EmptyTabTitle {
                window: rp.name.clone(),
            });
        }
    }
    let title = rt.title.clone().unwrap_or_else(|| basename(&dir));
    if !seen_titles.insert(title.clone()) {
        return Err(ResolveError::DuplicateTab {
            window: rp.name.clone(),
            title,
        });
    }
    if !dir.exists() {
        warnings.push(Warning {
            window: rp.name.clone(),
            message: format!("dir does not exist: {}", dir.display()),
        });
    }
    // `shell` and `cmd` cascade tab → window → global (nearest set level wins); `shell`
    // falls back to the built-in when unset everywhere, `cmd` is a startup command run
    // *inside* the shell (None = bare shell; `cmd = ""` at any level opts out of inheritance).
    let shell = cascade(rt.shell.as_deref(), rp.shell.as_deref(), global_shell)
        .unwrap_or(DEFAULT_SHELL)
        .to_string();
    let startup = cascade(rt.cmd.as_deref(), rp.cmd.as_deref(), global_cmd).map(String::from);
    Ok(Tab {
        key: title.clone(),
        title,
        dir,
        shell,
        startup,
        keep_alive: rt.keep_alive,
        group,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::parse;

    fn resolve_str(s: &str) -> Result<(Config, Vec<Warning>), ResolveError> {
        resolve(parse(s).unwrap())
    }

    #[test]
    fn title_defaults_to_dir_basename() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/locus"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].title, "locus");
        assert_eq!(cfg.windows[0].tabs[0].key, "locus");
    }

    #[test]
    fn global_shell_applies_and_cmd_becomes_startup() {
        let (cfg, _) = resolve_str(
            r##"
shell = "zsh"
[[window]]
name = "a"
colour = "#000000"
  [[window.tab]]
  dir = "/tmp/x"
[[window]]
name = "b"
colour = "#000000"
  [[window.tab]]
  dir = "/tmp/y"
  cmd = "tmux"
"##,
        )
        .unwrap();
        // Every tab runs the cascaded shell (here the global `shell`); a tab's `cmd` is its
        // startup command, run *inside* that shell rather than replacing it.
        assert_eq!(cfg.windows[0].tabs[0].shell, "zsh");
        assert_eq!(cfg.windows[0].tabs[0].startup, None);
        assert_eq!(cfg.windows[1].tabs[0].shell, "zsh");
        assert_eq!(cfg.windows[1].tabs[0].startup.as_deref(), Some("tmux"));

        // shell unset everywhere → built-in shell; empty cmd → no startup command.
        let (cfg2, _) = resolve_str(
            r##"
[[window]]
name = "a"
colour = "#000000"
  [[window.tab]]
  dir = "/tmp/x"
  cmd = "   "
"##,
        )
        .unwrap();
        assert_eq!(cfg2.windows[0].tabs[0].shell, DEFAULT_SHELL);
        assert_eq!(cfg2.windows[0].tabs[0].startup, None);
    }

    #[test]
    fn shell_and_cmd_cascade_with_nearest_level_winning() {
        let (cfg, _) = resolve_str(
            r##"
shell = "fish"
cmd = "global-cmd"
[[window]]
name = "work"
colour = "#000000"
shell = "zsh"
cmd = "amux"
  [[window.tab]]
  title = "inherits"
  dir = "/tmp/a"
  [[window.tab]]
  title = "overrides"
  dir = "/tmp/b"
  shell = "bash"
  cmd = "vim"
  [[window.tab]]
  title = "opts-out"
  dir = "/tmp/c"
  cmd = ""
[[window]]
name = "plain"
colour = "#000000"
  [[window.tab]]
  title = "from-global"
  dir = "/tmp/d"
"##,
        )
        .unwrap();
        let work = &cfg.windows[0].tabs;
        // No tab-level value → inherit the window's shell + cmd.
        assert_eq!(work[0].shell, "zsh");
        assert_eq!(work[0].startup.as_deref(), Some("amux"));
        // Tab-level values win over the window's.
        assert_eq!(work[1].shell, "bash");
        assert_eq!(work[1].startup.as_deref(), Some("vim"));
        // `cmd = ""` opts out of the inherited command (bare shell), but shell still cascades.
        assert_eq!(work[2].shell, "zsh");
        assert_eq!(work[2].startup, None);
        // A window that sets neither inherits the global shell + cmd.
        let plain = &cfg.windows[1].tabs[0];
        assert_eq!(plain.shell, "fish");
        assert_eq!(plain.startup.as_deref(), Some("global-cmd"));
    }

    #[test]
    fn empty_shell_opts_out_to_default_not_inherited() {
        // `shell = ""` is *set* (empty), so — exactly like `cmd = ""` — it opts the
        // tab out of inheriting the window's shell rather than falling through to it.
        // With nothing left in the cascade it resets to DEFAULT_SHELL, NOT "zsh".
        // Locks the asymmetric-looking semantics so a future "empty inherits" change
        // can't slip through (the `cmd = ""` case already has this guard).
        let (cfg, _) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
shell = "zsh"
  [[window.tab]]
  title = "opts-out"
  dir = "/tmp/a"
  shell = ""
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].shell, DEFAULT_SHELL);
    }

    #[test]
    fn nonexistent_dir_is_warning_not_error() {
        let (cfg, warns) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/no/such/path/zzz"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs.len(), 1);
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].window, "work");
        assert!(warns[0].message.contains("does not exist"));
    }

    #[test]
    fn duplicate_window_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "dup"
colour = "#000000"
[[window]]
name = "dup"
colour = "#000000"
"##,
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::DuplicateWindow("dup".into()));
    }

    #[test]
    fn duplicate_tab_title_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
  [[window.tab]]
  title = "same"
  dir = "/tmp/a"
  [[window.tab]]
  title = "same"
  dir = "/tmp/b"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateTab {
                window: "work".into(),
                title: "same".into()
            }
        );
    }

    #[test]
    fn empty_dir_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
  [[window.tab]]
  dir = "   "
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::EmptyDir {
                window: "work".into()
            }
        );
    }

    #[test]
    fn bad_colour_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "teal"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::BadColour { .. }));
    }

    #[test]
    fn root_dir_without_title_gets_nonempty_title() {
        let (cfg, _warns) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/"
"##,
        )
        .unwrap();
        let tab = &cfg.windows[0].tabs[0];
        assert_eq!(tab.title, "/");
        assert_eq!(tab.key, "/");
        assert!(!tab.title.is_empty());
    }

    #[test]
    fn tilde_in_dir_expands_to_home() {
        let (cfg, _warns) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "~/some/deep/path"
"##,
        )
        .unwrap();
        let home = dirs::home_dir().unwrap();
        let tab = &cfg.windows[0].tabs[0];
        assert_eq!(tab.dir, home.join("some/deep/path"));
        assert_eq!(tab.title, "path"); // basename of the expanded dir
    }

    #[test]
    fn empty_window_name_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "  "
colour = "#000000"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EmptyWindowName { index: 0 }));
    }

    #[test]
    fn empty_explicit_tab_title_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
  [[window.tab]]
  title = ""
  dir = "/tmp/a"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EmptyTabTitle { .. }));
    }

    #[test]
    fn loose_then_grouped_tabs_flatten_in_order_with_group_tags() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  title = "notes"
  dir = "/tmp/notes"
  [[window.group]]
  name = "frontend"
    [[window.group.tab]]
    title = "web"
    dir = "/tmp/web"
  [[window.group]]
  name = "backend"
    [[window.group.tab]]
    title = "api"
    dir = "/tmp/api"
"##,
        )
        .unwrap();
        let tabs = &cfg.windows[0].tabs;
        // Flat order: loose first, then groups in file order.
        let order: Vec<(&str, Option<&str>)> = tabs
            .iter()
            .map(|t| (t.title.as_str(), t.group.as_deref()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("notes", None),
                ("web", Some("frontend")),
                ("api", Some("backend")),
            ]
        );
    }

    #[test]
    fn empty_group_name_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
  [[window.group]]
  name = "  "
    [[window.group.tab]]
    dir = "/tmp/a"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EmptyGroupName { .. }));
    }

    #[test]
    fn duplicate_group_name_is_error() {
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
  [[window.group]]
  name = "dup"
    [[window.group.tab]]
    title = "a"
    dir = "/tmp/a"
  [[window.group]]
  name = "dup"
    [[window.group.tab]]
    title = "b"
    dir = "/tmp/b"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateGroup {
                window: "work".into(),
                group: "dup".into()
            }
        );
    }

    #[test]
    fn duplicate_title_across_loose_and_group_is_error() {
        // Title uniqueness is window-wide: a loose tab and a grouped tab can't share a title.
        let err = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#000000"
  [[window.tab]]
  title = "same"
  dir = "/tmp/a"
  [[window.group]]
  name = "g"
    [[window.group.tab]]
    title = "same"
    dir = "/tmp/b"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateTab {
                window: "work".into(),
                title: "same".into()
            }
        );
    }

    #[test]
    fn loose_tab_has_no_group() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/locus"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].group, None);
    }

    #[test]
    fn tilde_in_icon_expands_to_home() {
        let (cfg, _warns) = resolve_str(
            r##"
[[window]]
name = "work"
colour = "#0f8a8a"
icon = "~/some/icon.png"
  [[window.tab]]
  dir = "/tmp/x"
"##,
        )
        .unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(cfg.windows[0].icon, Some(home.join("some/icon.png")));
    }
}
