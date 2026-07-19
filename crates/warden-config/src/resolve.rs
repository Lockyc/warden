use crate::model::{Config, Density, Root, Tab, TabDigitKeys, Warning, Window};
use crate::raw::{RawConfig, RawWindow};
use crate::{Colour, ColourError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Last-resort shell when no `shell` is set at any level *and* the caller injected none.
/// warden is a terminal, so the app/CLI detect the user's **login shell** at runtime and
/// pass it as the default via [`resolve_with`]/[`load_with`] — a terminal runs your login
/// shell. This neutral macOS fallback only applies to the bare [`resolve`]/[`load`] API
/// (tests, mainly) or if that detection fails. Spawned as a **login shell** (`-l`) so it
/// sources config and builds PATH, exactly as a terminal would. Each tab runs the cascaded
/// `shell`; a tab's cascaded `cmd`, if any, is auto-run *inside* it.
pub const DEFAULT_SHELL: &str = "/bin/zsh -l";

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
    #[error("window {window:?} has two tabs with the same identity: {identity:?} (give one an `id` to disambiguate, or change its dir)")]
    DuplicateTabIdentity { window: String, identity: String },
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
    #[error("invalid density {0:?} (expected \"comfortable\" or \"compact\")")]
    BadDensity(String),
    #[error("window {window:?} has a root with an empty dir")]
    EmptyRootDir { window: String },
    #[error("window {window:?} has a root with an empty name")]
    EmptyRootName { window: String },
    #[error("window {window:?} has duplicate section name: {name:?}")]
    DuplicateSection { window: String, name: String },
    #[error("window {window:?} has a root with invalid depth {depth} (must be >= 1)")]
    InvalidRootDepth { window: String, depth: u32 },
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

/// Parse the global `density` setting. Missing/empty → the default
/// (`Comfortable`); an unrecognised value is an error rather than a silent fallback.
fn resolve_density(raw: Option<&str>) -> Result<Density, ResolveError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(Density::default()),
        Some("comfortable") => Ok(Density::Comfortable),
        Some("compact") => Ok(Density::Compact),
        Some(other) => Err(ResolveError::BadDensity(other.to_string())),
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
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

/// Render a resolved dir to its identity string: the lossy path with any single trailing
/// separator stripped (so `~/a` and `~/a/` are one identity) — except a bare root `/`.
/// Must match the scanner's discovered-tab key (`path.to_string_lossy()`, never trailing-
/// slashed) so a curated tab and a same-dir discovered project share one key and the curated
/// one shadows the discovered one in `effective_config`.
fn normalize_dir_key(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    let trimmed = s.strip_suffix('/').filter(|t| !t.is_empty()).unwrap_or(&s);
    trimmed.to_string()
}

/// Resolve with the built-in [`DEFAULT_SHELL`] as the unset-`shell` fallback. Convenience
/// for tests and the bare [`load`]; the app/CLI use [`resolve_with`] to inject the user's
/// detected login shell instead.
pub fn resolve(raw: RawConfig) -> Result<(Config, Vec<Warning>), ResolveError> {
    resolve_with(raw, DEFAULT_SHELL)
}

/// Resolve a raw config, defaulting an unset `shell` (at every cascade level) to
/// `default_shell` — the caller's detected **login shell**. warden is a terminal, so the app
/// passes your `$SHELL` here; this keeps the crate pure (no env access) by taking the default
/// as data.
pub fn resolve_with(
    raw: RawConfig,
    default_shell: &str,
) -> Result<(Config, Vec<Warning>), ResolveError> {
    let global_shell = raw.shell.as_deref();
    let global_cmd = raw.cmd.as_deref();
    let global_probe = raw.probe.as_deref();
    let global_kill = raw.kill.as_deref();
    let mut warnings = Vec::new();
    let mut windows = Vec::with_capacity(raw.windows.len());
    let mut seen_windows = HashSet::new();
    let tab_digit_keys = resolve_tab_digit_keys(raw.tab_digit_keys.as_deref())?;
    let density = resolve_density(raw.density.as_deref())?;

    for (index, rp) in raw.windows.iter().enumerate() {
        // Dedup on the trimmed title so a trailing-space typo collides rather than
        // creating a distinct window; resolve_window stores it trimmed too (below).
        let title = rp.title.trim();
        if title.is_empty() {
            return Err(ResolveError::EmptyWindowTitle { index });
        }
        if !seen_windows.insert(title.to_string()) {
            return Err(ResolveError::DuplicateWindow(title.to_string()));
        }
        windows.push(resolve_window(
            rp,
            default_shell,
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
            density,
            sidebar_drag: raw.sidebar_drag.unwrap_or(true),
            auto_update: raw.auto_update.unwrap_or(true),
            notify_debug: raw.notify_debug.unwrap_or(false),
        },
        warnings,
    ))
}

#[allow(clippy::too_many_arguments)]
fn resolve_window(
    rp: &RawWindow,
    default_shell: &str,
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
    let open_on_start = rp.open_on_start.unwrap_or(true);
    // Flatten loose tabs + each group's tabs into one ordered list: loose first
    // (ungrouped, headerless), then each `[[window.group]]` in file order, tabs
    // within a group keeping file order. Groups add no cascade level — they're
    // presentation only — so every tab resolves identically (tab → window → global)
    // and just carries its group name. Identity uniqueness (id-else-dir) is window-wide
    // (shared `seen_keys` across loose + grouped tabs); titles may repeat.
    let total: usize = rp.tabs.len() + rp.groups.iter().map(|g| g.tabs.len()).sum::<usize>();
    let mut tabs = Vec::with_capacity(total);
    let mut seen_keys = HashSet::new();

    for rt in &rp.tabs {
        tabs.push(resolve_tab(
            rt,
            None,
            rp,
            default_shell,
            global_shell,
            global_cmd,
            global_probe,
            global_kill,
            &mut seen_keys,
            warnings,
        )?);
    }

    // Section names (group names + root names) share one uniqueness namespace: a group
    // and a root can't share a name, matching how both render as labelled sidebar
    // sections. A group-vs-group clash still reports `DuplicateGroup`; any other clash
    // (root-vs-root or cross-kind) reports `DuplicateSection`.
    let mut seen_sections = HashSet::new();
    for g in &rp.groups {
        // Dedup + tag tabs with the trimmed name so a trailing-space typo collides
        // and Tab.group matches the section name the sidebar renders.
        let group_name = g.name.trim();
        if group_name.is_empty() {
            return Err(ResolveError::EmptyGroupName {
                window: rp.title.clone(),
            });
        }
        if !seen_sections.insert(group_name.to_string()) {
            return Err(ResolveError::DuplicateGroup {
                window: rp.title.clone(),
                group: group_name.to_string(),
            });
        }
        for rt in &g.tabs {
            tabs.push(resolve_tab(
                rt,
                Some(group_name.to_string()),
                rp,
                default_shell,
                global_shell,
                global_cmd,
                global_probe,
                global_kill,
                &mut seen_keys,
                warnings,
            )?);
        }
    }

    let mut roots = Vec::with_capacity(rp.roots.len());
    for rr in &rp.roots {
        let root = resolve_root(
            rr,
            rp,
            default_shell,
            global_shell,
            global_cmd,
            global_probe,
            global_kill,
            warnings,
        )?;
        if !seen_sections.insert(root.name.clone()) {
            return Err(ResolveError::DuplicateSection {
                window: rp.title.clone(),
                name: root.name.clone(),
            });
        }
        roots.push(root);
    }

    Ok(Window {
        title: rp.title.trim().to_string(),
        colour,
        width,
        height,
        open_on_start,
        tabs,
        roots,
    })
}

/// Resolve one raw tab into a `Tab`, tagged with `group` (`None` = loose/ungrouped).
/// Shared by the loose-tab and grouped-tab passes so both validate and cascade
/// identically; `seen_keys` is threaded in to enforce window-wide identity (id-else-dir)
/// uniqueness.
#[allow(clippy::too_many_arguments)]
fn resolve_tab(
    rt: &crate::raw::RawTab,
    group: Option<String>,
    rp: &RawWindow,
    default_shell: &str,
    global_shell: Option<&str>,
    global_cmd: Option<&str>,
    global_probe: Option<&str>,
    global_kill: Option<&str>,
    seen_keys: &mut HashSet<String>,
    warnings: &mut Vec<Warning>,
) -> Result<Tab, ResolveError> {
    let dir_str = rt.dir.trim();
    if dir_str.is_empty() {
        return Err(ResolveError::EmptyDir {
            window: rp.title.clone(),
        });
    }
    let dir = expand_tilde(dir_str);
    if let Some(ref t) = rt.title {
        if t.trim().is_empty() {
            return Err(ResolveError::EmptyTabTitle {
                window: rp.title.clone(),
            });
        }
    }
    // Title is a pure display label now — trimmed, defaulting to the dir basename, and NOT
    // deduplicated (titles may repeat window-wide).
    let title = rt
        .title
        .as_deref()
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|| basename(&dir));
    // Identity: explicit non-empty `id`, else the normalized dir. Empty `id = ""` = unset.
    let id = rt
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let key = id.clone().unwrap_or_else(|| normalize_dir_key(&dir));
    if !seen_keys.insert(key.clone()) {
        return Err(ResolveError::DuplicateTabIdentity {
            window: rp.title.clone(),
            identity: key,
        });
    }
    if !dir.exists() {
        warnings.push(Warning {
            window: rp.title.clone(),
            message: format!("dir does not exist: {}", dir.display()),
        });
    }
    // `shell` and `cmd` cascade tab → window → global (nearest set level wins); `shell`
    // falls back to `default_shell` (the caller's detected login shell) when unset everywhere,
    // `cmd` is a startup command run *inside* the shell (None = bare shell; `cmd = ""` at any
    // level opts out of inheritance).
    let shell = cascade(rt.shell.as_deref(), rp.shell.as_deref(), global_shell)
        .unwrap_or(default_shell)
        .to_string();
    let startup = cascade(rt.cmd.as_deref(), rp.cmd.as_deref(), global_cmd).map(String::from);
    let probe = cascade(rt.probe.as_deref(), rp.probe.as_deref(), global_probe).map(String::from);
    let kill = cascade(rt.kill.as_deref(), rp.kill.as_deref(), global_kill).map(String::from);
    Ok(Tab {
        id,
        key,
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

/// Map config-core's leaf-free `RootError` onto warden's own `ResolveError` variants, adding the
/// enclosing window's context (config-core has no window concept).
fn map_root_error(e: config_core::RootError, window: &str) -> ResolveError {
    match e {
        config_core::RootError::EmptyDir => ResolveError::EmptyRootDir {
            window: window.to_string(),
        },
        config_core::RootError::EmptyName => ResolveError::EmptyRootName {
            window: window.to_string(),
        },
        config_core::RootError::ZeroDepth(depth) => ResolveError::InvalidRootDepth {
            window: window.to_string(),
            depth,
        },
    }
}

/// Resolve one raw root into a `Root`. Mirrors `resolve_tab`'s cascade but with no
/// tab level (root → window → global) since a root has no per-tab config of its own.
#[allow(clippy::too_many_arguments)]
fn resolve_root(
    rr: &crate::raw::RawRoot,
    rp: &RawWindow,
    default_shell: &str,
    global_shell: Option<&str>,
    global_cmd: Option<&str>,
    global_probe: Option<&str>,
    global_kill: Option<&str>,
    warnings: &mut Vec<Warning>,
) -> Result<Root, ResolveError> {
    // name/dir/depth validation + tilde expansion + basename default are shared with lector via
    // config-core's `resolve_root_dir` — leaf-free, so it hands back a bare RootDir and warden
    // maps its error onto ResolveError with this window's context.
    let root_dir = config_core::resolve_root_dir(rr.name.as_deref(), &rr.dir, rr.depth)
        .map_err(|e| map_root_error(e, &rp.title))?;
    if !root_dir.dir.exists() {
        warnings.push(Warning {
            window: rp.title.clone(),
            message: format!("root dir does not exist: {}", root_dir.dir.display()),
        });
    }
    // Cascade root→window→global (no tab level); shell falls back to the login shell.
    let shell = cascade(rr.shell.as_deref(), rp.shell.as_deref(), global_shell)
        .unwrap_or(default_shell)
        .to_string();
    let startup = cascade(rr.cmd.as_deref(), rp.cmd.as_deref(), global_cmd).map(String::from);
    let probe = cascade(rr.probe.as_deref(), rp.probe.as_deref(), global_probe).map(String::from);
    let kill = cascade(rr.kill.as_deref(), rp.kill.as_deref(), global_kill).map(String::from);
    Ok(Root {
        name: root_dir.name,
        dir: root_dir.dir,
        depth: root_dir.depth,
        shell,
        startup,
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
    fn notify_debug_defaults_false() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(!cfg.notify_debug);
    }

    #[test]
    fn notify_debug_parses_true() {
        let (cfg, _) = resolve_str(
            r##"
notify_debug = true
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(cfg.notify_debug);
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
    fn density_defaults_to_comfortable() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert_eq!(cfg.density, Density::Comfortable);
    }

    #[test]
    fn density_parses_compact() {
        let (cfg, _) = resolve_str(
            r##"
density = "compact"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert_eq!(cfg.density, Density::Compact);
    }

    #[test]
    fn density_rejects_unknown() {
        let err = resolve_str(
            r##"
density = "roomy"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::BadDensity("roomy".to_string()));
    }

    #[test]
    fn sidebar_drag_defaults_to_true() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(cfg.sidebar_drag);
    }

    #[test]
    fn sidebar_drag_can_be_disabled() {
        let (cfg, _) = resolve_str(
            r##"
sidebar_drag = false
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(!cfg.sidebar_drag);
    }

    #[test]
    fn auto_update_defaults_to_true() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(cfg.auto_update);
    }

    #[test]
    fn auto_update_can_be_disabled() {
        let (cfg, _) = resolve_str(
            r##"
auto_update = false
[[window]]
title = "w"
colour = "#0f8a8a"
"##,
        )
        .unwrap();
        assert!(!cfg.auto_update);
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
        assert_eq!(cfg.windows[0].tabs[0].key, "/tmp/alpha");
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
    fn duplicate_tab_title_with_different_dirs_is_no_longer_an_error() {
        // Titles are a pure display label now — two tabs may share a title as long
        // as their identities (id-else-dir) differ.
        let (cfg, _w) = resolve_str(
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
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].title, "same");
        assert_eq!(cfg.windows[0].tabs[1].title, "same");
        assert_eq!(cfg.windows[0].tabs[0].key, "/tmp/a");
        assert_eq!(cfg.windows[0].tabs[1].key, "/tmp/b");
    }

    #[test]
    fn same_basename_over_different_dirs_is_no_longer_an_error() {
        // Two tabs in different dirs but the same basename and no explicit title both
        // default to that basename — that's fine now: identity is the dir, not the
        // title, so the shared default title doesn't collide.
        let (cfg, _w) = resolve_str(
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
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].title, "alpha");
        assert_eq!(cfg.windows[0].tabs[1].title, "alpha");
        assert_eq!(cfg.windows[0].tabs[0].key, "/a/alpha");
        assert_eq!(cfg.windows[0].tabs[1].key, "/b/alpha");
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
    fn duplicate_title_across_loose_and_group_is_no_longer_an_error() {
        // A loose tab and a grouped tab may share a title now — identity is window-wide
        // id-else-dir, and titles aren't part of it.
        let (cfg, _w) = resolve_str(
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
        .unwrap();
        assert_eq!(cfg.windows[0].tabs[0].title, "same");
        assert_eq!(cfg.windows[0].tabs[1].title, "same");
        assert_eq!(cfg.windows[0].tabs[0].key, "/tmp/a");
        assert_eq!(cfg.windows[0].tabs[1].key, "/tmp/b");
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
  dir = "/tmp/a"
  title = "inherits-window"

  [[window.tab]]
  dir = "/tmp/b"
  title = "own-kill"
  kill = "tab-kill {title}"

  [[window.tab]]
  dir = "/tmp/c"
  title = "opts-out"
  kill = ""

[[window]]
title = "w2"
colour = "#0f8a8a"

  [[window.tab]]
  dir = "/tmp"
  title = "inherits-global"
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
        // global reaches a tab when no window/tab level is set — exercises the
        // `global_kill` threading directly (w1 masks it with a window-level kill).
        // The cascaded value is stored raw; `{dir}` is substituted at run time, not here.
        assert_eq!(
            cfg.windows[1].tabs[0].kill.as_deref(),
            Some("global-kill {dir}")
        );
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
    fn root_cascade_resolves_from_window_and_global() {
        let (cfg, _) = resolve_str(
            r##"
shell = "gsh -l"
probe = "global-probe"

[[window]]
title = "dev"
probe = "win-probe"

  [[window.root]]
  dir = "~/Developer"
  cmd = "run"
"##,
        )
        .unwrap();
        let r = &cfg.windows[0].roots[0];
        assert_eq!(r.name, "Developer"); // defaulted from basename
        assert_eq!(r.depth, 6); // default depth
        assert_eq!(r.shell, "gsh -l"); // from global
        assert_eq!(r.startup.as_deref(), Some("run"));
        assert_eq!(r.probe.as_deref(), Some("win-probe")); // window beats global
        assert!(r.kill.is_none());
    }

    #[test]
    fn root_empty_probe_opts_out_of_inherited() {
        let (cfg, _) = resolve_str(
            r##"
probe = "global-probe"
[[window]]
title = "dev"
  [[window.root]]
  dir = "~/x"
  probe = ""
"##,
        )
        .unwrap();
        assert!(cfg.windows[0].roots[0].probe.is_none());
    }

    #[test]
    fn root_name_collides_with_group_name_errors() {
        let err = resolve_str(
            r##"
[[window]]
title = "dev"
  [[window.group]]
  name = "shared"
    [[window.group.tab]]
    dir = "~/a"
  [[window.root]]
  name = "shared"
  dir = "~/b"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateSection {
                window: "dev".into(),
                name: "shared".into()
            }
        );
    }

    #[test]
    fn root_bad_depth_errors() {
        let err = resolve_str(
            r##"
[[window]]
title = "dev"
  [[window.root]]
  dir = "~/x"
  depth = 0
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::InvalidRootDepth {
                window: "dev".into(),
                depth: 0
            }
        );
    }

    #[test]
    fn root_empty_dir_errors() {
        let err = resolve_str(
            r##"
[[window]]
title = "dev"
  [[window.root]]
  dir = ""
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::EmptyRootDir {
                window: "dev".into()
            }
        );
    }

    #[test]
    fn root_empty_name_errors() {
        let err = resolve_str(
            r##"
[[window]]
title = "dev"
  [[window.root]]
  name = ""
  dir = "~/x"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::EmptyRootName {
                window: "dev".into()
            }
        );
    }

    #[test]
    fn root_missing_dir_warns_not_errors() {
        let (cfg, warnings) = resolve_str(
            r##"
[[window]]
title = "dev"
  [[window.root]]
  dir = "/no/such/warden-root-xyz"
"##,
        )
        .unwrap();
        // The root still resolves (the tree just scans nothing); a missing dir is a
        // warning, mirroring a missing tab dir.
        assert_eq!(cfg.windows[0].roots.len(), 1);
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("root dir does not exist")));
    }

    #[test]
    fn root_vs_root_duplicate_name_errors() {
        let err = resolve_str(
            r##"
[[window]]
title = "dev"
  [[window.root]]
  name = "dup"
  dir = "~/a"
  [[window.root]]
  name = "dup"
  dir = "~/b"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateSection {
                window: "dev".into(),
                name: "dup".into()
            }
        );
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

    // Window titles are validated for emptiness after trimming, so they must also be
    // deduped and stored trimmed — otherwise a trailing-space typo ("work" vs "work ")
    // slips past uniqueness as a distinct key, and later fixing the space reads as a
    // Window title change → destructive (close+reopen). Tab titles are trimmed the same
    // way for display, but no longer feed identity — see the dir-collision test below.

    #[test]
    fn window_titles_differing_only_by_whitespace_collide() {
        let err = resolve_str(
            r##"
[[window]]
title = "work"
  [[window.tab]]
  dir = "/tmp"

[[window]]
title = "work "
  [[window.tab]]
  dir = "/tmp"
"##,
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::DuplicateWindow("work".into()));
    }

    #[test]
    fn tabs_sharing_a_dir_without_id_collide_regardless_of_title() {
        // Both tabs resolve to the same dir "/tmp", so their identity (id-else-dir)
        // collides — independent of the (here-irrelevant) title whitespace difference.
        let err = resolve_str(
            r##"
[[window]]
title = "work"
  [[window.tab]]
  title = "api"
  dir = "/tmp"
  [[window.tab]]
  title = "api "
  dir = "/tmp"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateTabIdentity {
                window: "work".into(),
                identity: "/tmp".into(),
            }
        );
    }

    #[test]
    fn explicit_title_is_stored_trimmed() {
        let (cfg, _) = resolve_str(
            r##"
[[window]]
title = "  work  "
  [[window.tab]]
  title = "  api  "
  dir = "/tmp"
"##,
        )
        .unwrap();
        assert_eq!(cfg.windows[0].title, "work");
        assert_eq!(cfg.windows[0].tabs[0].title, "api");
        assert_eq!(cfg.windows[0].tabs[0].key, "/tmp");
    }

    #[test]
    fn section_names_differing_only_by_whitespace_collide() {
        let err = resolve_str(
            r##"
[[window]]
title = "work"
  [[window.group]]
  name = "backend"
    [[window.group.tab]]
    dir = "/tmp"
  [[window.group]]
  name = "backend "
    [[window.group.tab]]
    dir = "/tmp"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateGroup {
                window: "work".into(),
                group: "backend".into(),
            }
        );
    }

    #[test]
    fn open_on_start_defaults_to_true() {
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
        assert!(cfg.windows[0].open_on_start);
    }

    #[test]
    fn open_on_start_explicit_false_and_true() {
        let cfg = resolve(
            parse(
                r##"
[[window]]
title = "a"
open_on_start = false
  [[window.tab]]
  dir = "/tmp"

[[window]]
title = "b"
open_on_start = true
  [[window.tab]]
  dir = "/tmp"
"##,
            )
            .unwrap(),
        )
        .unwrap()
        .0;
        assert!(!cfg.windows[0].open_on_start);
        assert!(cfg.windows[1].open_on_start);
    }

    #[test]
    fn identity_is_dir_when_no_id_and_titles_may_repeat() {
        // Two tabs, same title, different dirs → both resolve (titles no longer unique),
        // keys are the normalized dirs.
        let (cfg, _w) = resolve(
            parse(
                r##"
[[window]]
title = "w"
  [[window.tab]]
  title = "same"
  dir = "/tmp/a"
  [[window.tab]]
  title = "same"
  dir = "/tmp/b"
"##,
            )
            .unwrap(),
        )
        .unwrap();
        let keys: Vec<&str> = cfg.windows[0].tabs.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["/tmp/a", "/tmp/b"]);
        assert_eq!(cfg.windows[0].tabs[0].id, None);
    }

    #[test]
    fn duplicate_dir_without_id_is_an_error() {
        let err = resolve(
            parse(
                r##"
[[window]]
title = "w"
  [[window.tab]]
  dir = "/tmp/a"
  [[window.tab]]
  dir = "/tmp/a"
"##,
            )
            .unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateTabIdentity {
                window: "w".into(),
                identity: "/tmp/a".into()
            }
        );
    }

    #[test]
    fn explicit_ids_disambiguate_a_shared_dir() {
        let (cfg, _w) = resolve(
            parse(
                r##"
[[window]]
title = "w"
  [[window.tab]]
  id = "server"
  dir = "/tmp/a"
  [[window.tab]]
  id = "shell"
  dir = "/tmp/a"
"##,
            )
            .unwrap(),
        )
        .unwrap();
        let keys: Vec<&str> = cfg.windows[0].tabs.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["server", "shell"]);
    }

    #[test]
    fn empty_id_falls_back_to_dir() {
        let (cfg, _w) = resolve(
            parse(
                r##"
[[window]]
title = "w"
  [[window.tab]]
  id = ""
  dir = "/tmp/a/"
"##,
            )
            .unwrap(),
        )
        .unwrap();
        // empty id = unset; trailing slash normalized away.
        assert_eq!(cfg.windows[0].tabs[0].key, "/tmp/a");
        assert_eq!(cfg.windows[0].tabs[0].id, None);
    }
}
