use crate::model::{Config, Tab, TabDigitKeys, Warning, Window};
use crate::raw::{RawConfig, RawWindow};
use crate::{Colour, ColourError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// The shell spawned in a tab when no `shell` is set at any level. Each tab runs the
/// cascaded `shell`; a tab's cascaded `cmd`, if any, is auto-run *inside* it.
pub const DEFAULT_SHELL: &str = "fish -l";

/// Window accent used when `colour` is omitted — a neutral grey so the banner
/// still renders identity without an accent. (curator parity: omit → neutral.)
pub const DEFAULT_COLOUR: Colour = Colour {
    r: 0x6b,
    g: 0x72,
    b: 0x80,
};

/// Default window width when `width` is omitted. Matches curator's default.
pub const DEFAULT_WIDTH: u32 = 1500;

/// Default window height when `height` is omitted. Matches curator's default.
pub const DEFAULT_HEIGHT: u32 = 1000;

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
    #[error("duplicate window title: {0:?}")]
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
    #[error("window at index {index} has an empty title")]
    EmptyWindowTitle { index: usize },
    #[error("window {window:?} has a tab with an empty explicit title")]
    EmptyTabTitle { window: String },
    #[error("window {window:?} has a group with an empty name")]
    EmptyGroupName { window: String },
    #[error("window {window:?} has duplicate group: {group:?}")]
    DuplicateGroup { window: String, group: String },
    #[error("window {window:?} has invalid size {width}x{height} (must be > 0)")]
    InvalidWindowSize {
        window: String,
        width: u32,
        height: u32,
    },
    #[error("invalid tab_digit_keys {0:?} (expected \"jump\" or \"cycle\")")]
    BadTabDigitKeys(String),
}

/// Parse the global `tab_digit_keys` setting. Missing/empty → the default
/// (`Jump`); an unrecognised value is an error rather than a silent fallback.
fn resolve_tab_digit_keys(raw: Option<&str>) -> Result<TabDigitKeys, ResolveError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(TabDigitKeys::default()),
        Some("jump") => Ok(TabDigitKeys::Jump),
        Some("cycle") => Ok(TabDigitKeys::Cycle),
        Some(other) => Err(ResolveError::BadTabDigitKeys(other.to_string())),
    }
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
    let global_probe = raw.probe.as_deref();
    let global_kill = raw.kill.as_deref();
    let mut warnings = Vec::new();
    let mut windows = Vec::with_capacity(raw.windows.len());
    let mut seen_windows = HashSet::new();
    let tab_digit_keys = resolve_tab_digit_keys(raw.tab_digit_keys.as_deref())?;

    for (index, rp) in raw.windows.iter().enumerate() {
        if rp.title.trim().is_empty() {
            return Err(ResolveError::EmptyWindowTitle { index });
        }
        if !seen_windows.insert(rp.title.clone()) {
            return Err(ResolveError::DuplicateWindow(rp.title.clone()));
        }
        windows.push(resolve_window(
            rp,
            global_shell,
            global_cmd,
            global_probe,
            global_kill,
            &mut warnings,
        )?);
    }
    Ok((
        Config {
            windows,
            format_on_save: raw.format_on_save.unwrap_or(false),
            tab_digit_keys,
            probe_interval: raw.probe_interval.unwrap_or(5),
        },
        warnings,
    ))
}

fn resolve_window(
    rp: &RawWindow,
    global_shell: Option<&str>,
    global_cmd: Option<&str>,
    global_probe: Option<&str>,
    global_kill: Option<&str>,
    warnings: &mut Vec<Warning>,
) -> Result<Window, ResolveError> {
    let colour = match rp.colour.as_deref() {
        None => DEFAULT_COLOUR,
        Some(s) => Colour::parse(s).map_err(|source| ResolveError::BadColour {
            window: rp.title.clone(),
            source,
        })?,
    };
    let width = rp.width.unwrap_or(DEFAULT_WIDTH);
    let height = rp.height.unwrap_or(DEFAULT_HEIGHT);
    if width == 0 || height == 0 {
        return Err(ResolveError::InvalidWindowSize {
            window: rp.title.clone(),
            width,
            height,
        });
    }
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
            global_probe,
            global_kill,
            &mut seen_titles,
            warnings,
        )?);
    }

    let mut seen_groups = HashSet::new();
    for g in &rp.groups {
        if g.name.trim().is_empty() {
            return Err(ResolveError::EmptyGroupName {
                window: rp.title.clone(),
            });
        }
        if !seen_groups.insert(g.name.clone()) {
            return Err(ResolveError::DuplicateGroup {
                window: rp.title.clone(),
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
                global_probe,
                global_kill,
                &mut seen_titles,
                warnings,
            )?);
        }
    }

    Ok(Window {
        title: rp.title.clone(),
        colour,
        width,
        height,
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
    global_probe: Option<&str>,
    global_kill: Option<&str>,
    seen_titles: &mut HashSet<String>,
    warnings: &mut Vec<Warning>,
) -> Result<Tab, ResolveError> {
    if rt.dir.trim().is_empty() {
        return Err(ResolveError::EmptyDir {
            window: rp.title.clone(),
        });
    }
    let dir = expand_tilde(&rt.dir);
    if let Some(ref t) = rt.title {
        if t.trim().is_empty() {
            return Err(ResolveError::EmptyTabTitle {
                window: rp.title.clone(),
            });
        }
    }
    let title = rt.title.clone().unwrap_or_else(|| basename(&dir));
    if !seen_titles.insert(title.clone()) {
        return Err(ResolveError::DuplicateTab {
            window: rp.title.clone(),
            title,
        });
    }
    if !dir.exists() {
        warnings.push(Warning {
            window: rp.title.clone(),
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
    let probe = cascade(rt.probe.as_deref(), rp.probe.as_deref(), global_probe).map(String::from);
    let kill = cascade(rt.kill.as_deref(), rp.kill.as_deref(), global_kill).map(String::from);
    Ok(Tab {
        key: title.clone(),
        title,
        dir,
        shell,
        startup,
        load_on_open: rt.load_on_open,
        group,
        probe,
        kill,
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
    fn format_on_save_defaults_false() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(!cfg.format_on_save);
    }

    #[test]
    fn format_on_save_parses_true() {
        let (cfg, _) = resolve_str(
            r##"
format_on_save = true
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(cfg.format_on_save);
    }

    #[test]
    fn tab_digit_keys_defaults_to_jump() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert_eq!(cfg.tab_digit_keys, TabDigitKeys::Jump);
    }

    #[test]
    fn tab_digit_keys_parses_cycle() {
        let (cfg, _) = resolve_str(
            r##"
tab_digit_keys = "cycle"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert_eq!(cfg.tab_digit_keys, TabDigitKeys::Cycle);
    }

    #[test]
    fn tab_digit_keys_rejects_unknown() {
        let err = resolve_str(
            r##"
tab_digit_keys = "wiggle"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::BadTabDigitKeys("wiggle".to_string()));
    }

    #[test]
    fn title_defaults_to_dir_basename() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/alpha"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].title, "alpha");
        assert_eq!(cfg.windows[0].tabs[0].key, "alpha");
    }

    #[test]
    fn global_shell_applies_and_cmd_becomes_startup() {
        let (cfg, _) = resolve_str(
            r##"
shell = "zsh"
[[window]]
title = "a"
colour = "#000000"
  [[window.tab]]
  dir = "/tmp/x"
[[window]]
title = "b"
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
title = "a"
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
title = "work"
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
title = "plain"
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
title = "work"
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
    fn window_level_empty_opts_out_of_global() {
        // The opt-out (`= ""` resets to None) must fire at the *window* level too, not
        // just the tab level: a window `shell`/`cmd = ""` opts the whole window out of
        // the global value rather than inheriting it. A tab under it that sets neither
        // then sees DEFAULT_SHELL / no startup, not the global "fish"/"global-cmd".
        let (cfg, _) = resolve_str(
            r##"
shell = "fish"
cmd = "global-cmd"
[[window]]
title = "bare"
colour = "#000000"
shell = ""
cmd = ""
  [[window.tab]]
  dir = "/tmp/a"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].shell, DEFAULT_SHELL);
        assert_eq!(cfg.windows[0].tabs[0].startup, None);
    }

    #[test]
    fn nonexistent_dir_is_warning_not_error() {
        let (cfg, warns) = resolve_str(
            r##"
[[window]]
title = "work"
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
title = "dup"
colour = "#000000"
[[window]]
title = "dup"
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
title = "work"
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
    fn duplicate_title_via_basename_collision_is_error() {
        // Two tabs in different dirs but the same basename and no explicit title both
        // default to that basename → DuplicateTab. Surprising-but-correct: titles are
        // unique window-wide and the default title is the dir basename. Pins it so the
        // default-title rule can't silently start tolerating collisions.
        let err = resolve_str(
            r##"
[[window]]
title = "work"
colour = "#000000"
  [[window.tab]]
  dir = "/a/alpha"
  [[window.tab]]
  dir = "/b/alpha"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateTab {
                window: "work".into(),
                title: "alpha".into()
            }
        );
    }

    #[test]
    fn empty_dir_is_error() {
        let err = resolve_str(
            r##"
[[window]]
title = "work"
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
title = "work"
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
title = "work"
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
title = "work"
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
    fn empty_window_title_is_error() {
        let err = resolve_str(
            r##"
[[window]]
title = "  "
colour = "#000000"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EmptyWindowTitle { index: 0 }));
    }

    #[test]
    fn empty_explicit_tab_title_is_error() {
        let err = resolve_str(
            r##"
[[window]]
title = "work"
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
title = "work"
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
title = "work"
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
title = "work"
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
title = "work"
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
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/alpha"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].group, None);
    }

    #[test]
    fn probe_cascades_with_nearest_level_winning() {
        let (cfg, _) = resolve_str(
            r##"
probe = "global-probe"
[[window]]
title = "work"
colour = "#000000"
probe = "win-probe"
  [[window.tab]]
  title = "inherits"
  dir = "/tmp/a"
  [[window.tab]]
  title = "overrides"
  dir = "/tmp/b"
  probe = "tab-probe"
  [[window.tab]]
  title = "opts-out"
  dir = "/tmp/c"
  probe = ""
[[window]]
title = "plain"
colour = "#000000"
  [[window.tab]]
  title = "from-global"
  dir = "/tmp/d"
"##,
        )
        .unwrap();
        let work = &cfg.windows[0].tabs;
        assert_eq!(work[0].probe.as_deref(), Some("win-probe")); // inherit window
        assert_eq!(work[1].probe.as_deref(), Some("tab-probe")); // tab wins
        assert_eq!(work[2].probe, None); // `probe = ""` opts out
        assert_eq!(
            cfg.windows[1].tabs[0].probe.as_deref(),
            Some("global-probe")
        );
    }

    #[test]
    fn probe_unset_everywhere_is_none() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#000000"
  [[window.tab]]
  dir = "/tmp/a"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].probe, None);
    }

    #[test]
    fn probe_interval_defaults_to_5_and_parses() {
        let (def, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#000000"
"##,
        )
        .unwrap();
        assert_eq!(def.probe_interval, 5);

        let (set, _) = resolve_str(
            r##"
probe_interval = 0
[[window]]
title = "w"
colour = "#000000"
"##,
        )
        .unwrap();
        assert_eq!(set.probe_interval, 0);
    }

    #[test]
    fn missing_colour_uses_neutral_default() {
        let cfg = resolve(
            parse(
                r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap()
        .0;
        assert_eq!(cfg.windows[0].colour, super::DEFAULT_COLOUR);
    }

    #[test]
    fn window_size_defaults_to_1500x1000() {
        let cfg = resolve(
            parse(
                r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap()
        .0;
        assert_eq!((cfg.windows[0].width, cfg.windows[0].height), (1500, 1000));
    }

    #[test]
    fn explicit_window_size_is_used() {
        let cfg = resolve(
            parse(
                r##"
[[window]]
title = "work"
width = 1200
height = 800
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap()
        .0;
        assert_eq!((cfg.windows[0].width, cfg.windows[0].height), (1200, 800));
    }

    #[test]
    fn zero_window_size_errors() {
        let err = resolve(
            parse(
                r##"
[[window]]
title = "work"
width = 0
height = 800
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::InvalidWindowSize { .. }));
    }

    #[test]
    fn zero_window_height_errors() {
        let err = resolve(
            parse(
                r#"
[[window]]
title = "work"
width = 1200
height = 0
  [[window.tab]]
  dir = "/tmp"
"#,
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::InvalidWindowSize { .. }));
    }

    #[test]
    fn kill_cascades_and_opts_out() {
        let raw = crate::raw::parse(
            r##"
kill = "global-kill {dir}"

[[window]]
title = "w"
colour = "#0f8a8a"
kill = "win-kill"

  [[window.tab]]
  dir = "/tmp"
  title = "inherits-window"

  [[window.tab]]
  dir = "/tmp"
  title = "own-kill"
  kill = "tab-kill {title}"

  [[window.tab]]
  dir = "/tmp"
  title = "opts-out"
  kill = ""
"##,
        )
        .unwrap();
        let (cfg, _) = resolve(raw).unwrap();
        let tabs = &cfg.windows[0].tabs;
        // window level wins over global when the tab is silent
        assert_eq!(tabs[0].kill.as_deref(), Some("win-kill"));
        // tab level wins over window
        assert_eq!(tabs[1].kill.as_deref(), Some("tab-kill {title}"));
        // explicit "" opts the tab out of the inherited window/global value
        assert_eq!(tabs[2].kill, None);
    }

    #[test]
    fn kill_defaults_to_none_when_unset_everywhere() {
        let raw = crate::raw::parse(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"

  [[window.tab]]
  dir = "/tmp"
  title = "t"
"##,
        )
        .unwrap();
        let (cfg, _) = resolve(raw).unwrap();
        assert_eq!(cfg.windows[0].tabs[0].kill, None);
    }

    #[test]
    fn present_invalid_colour_still_errors() {
        let err = resolve(
            parse(
                r##"
[[window]]
title = "work"
colour = "not-a-colour"
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::BadColour { .. }));
    }
}
