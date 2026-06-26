use crate::colour::{Colour, ColourError};
use crate::model::{Config, Profile, Tab, Warning};
use crate::raw::{RawConfig, RawProfile};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const DEFAULT_CMD: &str = "fish -l";

#[derive(Debug, Error, PartialEq)]
pub enum ResolveError {
    #[error("duplicate profile name: {0:?}")]
    DuplicateProfile(String),
    #[error("profile {profile:?} has duplicate tab title: {title:?}")]
    DuplicateTab { profile: String, title: String },
    #[error("profile {profile:?} has a tab with an empty dir")]
    EmptyDir { profile: String },
    #[error("profile {profile:?} has invalid colour")]
    BadColour {
        profile: String,
        #[source]
        source: ColourError,
    },
    #[error("profile at index {index} has an empty name")]
    EmptyProfileName { index: usize },
    #[error("profile {profile:?} has a tab with an empty explicit title")]
    EmptyTabTitle { profile: String },
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
    let default_cmd = raw.default_cmd.unwrap_or_else(|| DEFAULT_CMD.to_string());
    let mut warnings = Vec::new();
    let mut profiles = Vec::with_capacity(raw.profiles.len());
    let mut seen_profiles = HashSet::new();

    for (index, rp) in raw.profiles.into_iter().enumerate() {
        if rp.name.trim().is_empty() {
            return Err(ResolveError::EmptyProfileName { index });
        }
        if !seen_profiles.insert(rp.name.clone()) {
            return Err(ResolveError::DuplicateProfile(rp.name));
        }
        profiles.push(resolve_profile(rp, &default_cmd, &mut warnings)?);
    }
    Ok((Config { profiles }, warnings))
}

fn resolve_profile(
    rp: RawProfile,
    default_cmd: &str,
    warnings: &mut Vec<Warning>,
) -> Result<Profile, ResolveError> {
    let colour = Colour::parse(&rp.colour).map_err(|source| ResolveError::BadColour {
        profile: rp.name.clone(),
        source,
    })?;
    let icon = rp.icon.as_deref().map(expand_tilde);

    let mut tabs = Vec::with_capacity(rp.tabs.len());
    let mut seen_titles = HashSet::new();
    for rt in rp.tabs {
        if rt.dir.trim().is_empty() {
            return Err(ResolveError::EmptyDir { profile: rp.name.clone() });
        }
        let dir = expand_tilde(&rt.dir);
        if let Some(ref t) = rt.title {
            if t.trim().is_empty() {
                return Err(ResolveError::EmptyTabTitle { profile: rp.name.clone() });
            }
        }
        let title = rt.title.unwrap_or_else(|| basename(&dir));
        if !seen_titles.insert(title.clone()) {
            return Err(ResolveError::DuplicateTab {
                profile: rp.name.clone(),
                title,
            });
        }
        if !dir.exists() {
            warnings.push(Warning {
                profile: rp.name.clone(),
                message: format!("dir does not exist: {}", dir.display()),
            });
        }
        tabs.push(Tab {
            key: title.clone(),
            title,
            dir,
            cmd: rt.cmd.unwrap_or_else(|| default_cmd.to_string()),
            keep_alive: rt.keep_alive,
        });
    }

    Ok(Profile {
        name: rp.name,
        colour,
        icon,
        tabs,
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
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  dir = "/tmp/locus"
"##,
        )
        .unwrap();
        assert_eq!(cfg.profiles[0].tabs[0].title, "locus");
        assert_eq!(cfg.profiles[0].tabs[0].key, "locus");
    }

    #[test]
    fn cmd_falls_back_to_default_cmd_then_builtin() {
        let (cfg, _) = resolve_str(
            r##"
default_cmd = "zsh"
[[profile]]
name = "a"
colour = "#000000"
  [[profile.tab]]
  dir = "/tmp/x"
[[profile]]
name = "b"
colour = "#000000"
  [[profile.tab]]
  dir = "/tmp/y"
  cmd = "tmux"
"##,
        )
        .unwrap();
        assert_eq!(cfg.profiles[0].tabs[0].cmd, "zsh");
        assert_eq!(cfg.profiles[1].tabs[0].cmd, "tmux");

        let (cfg2, _) = resolve_str(
            r##"
[[profile]]
name = "a"
colour = "#000000"
  [[profile.tab]]
  dir = "/tmp/x"
"##,
        )
        .unwrap();
        assert_eq!(cfg2.profiles[0].tabs[0].cmd, DEFAULT_CMD);
    }

    #[test]
    fn nonexistent_dir_is_warning_not_error() {
        let (cfg, warns) = resolve_str(
            r##"
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  dir = "/no/such/path/zzz"
"##,
        )
        .unwrap();
        assert_eq!(cfg.profiles[0].tabs.len(), 1);
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].profile, "work");
        assert!(warns[0].message.contains("does not exist"));
    }

    #[test]
    fn duplicate_profile_is_error() {
        let err = resolve_str(
            r##"
[[profile]]
name = "dup"
colour = "#000000"
[[profile]]
name = "dup"
colour = "#000000"
"##,
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::DuplicateProfile("dup".into()));
    }

    #[test]
    fn duplicate_tab_title_is_error() {
        let err = resolve_str(
            r##"
[[profile]]
name = "work"
colour = "#000000"
  [[profile.tab]]
  title = "same"
  dir = "/tmp/a"
  [[profile.tab]]
  title = "same"
  dir = "/tmp/b"
"##,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::DuplicateTab { profile: "work".into(), title: "same".into() }
        );
    }

    #[test]
    fn empty_dir_is_error() {
        let err = resolve_str(
            r##"
[[profile]]
name = "work"
colour = "#000000"
  [[profile.tab]]
  dir = "   "
"##,
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::EmptyDir { profile: "work".into() });
    }

    #[test]
    fn bad_colour_is_error() {
        let err = resolve_str(
            r##"
[[profile]]
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
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  dir = "/"
"##,
        )
        .unwrap();
        let tab = &cfg.profiles[0].tabs[0];
        assert_eq!(tab.title, "/");
        assert_eq!(tab.key, "/");
        assert!(!tab.title.is_empty());
    }

    #[test]
    fn tilde_in_dir_expands_to_home() {
        let (cfg, _warns) = resolve_str(
            r##"
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  dir = "~/some/deep/path"
"##,
        )
        .unwrap();
        let home = dirs::home_dir().unwrap();
        let tab = &cfg.profiles[0].tabs[0];
        assert_eq!(tab.dir, home.join("some/deep/path"));
        assert_eq!(tab.title, "path"); // basename of the expanded dir
    }

    #[test]
    fn empty_profile_name_is_error() {
        let err = resolve_str(
            r##"
[[profile]]
name = "  "
colour = "#000000"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EmptyProfileName { index: 0 }));
    }

    #[test]
    fn empty_explicit_tab_title_is_error() {
        let err = resolve_str(
            r##"
[[profile]]
name = "work"
colour = "#000000"
  [[profile.tab]]
  title = ""
  dir = "/tmp/a"
"##,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EmptyTabTitle { .. }));
    }

    #[test]
    fn tilde_in_icon_expands_to_home() {
        let (cfg, _warns) = resolve_str(
            r##"
[[profile]]
name = "work"
colour = "#0f8a8a"
icon = "~/some/icon.png"
  [[profile.tab]]
  dir = "/tmp/x"
"##,
        )
        .unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(cfg.profiles[0].icon, Some(home.join("some/icon.png")));
    }
}
