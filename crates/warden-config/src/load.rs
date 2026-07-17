use crate::model::{Config, Warning};
use crate::raw::parse;
use crate::resolve::{resolve_with, ResolveError, DEFAULT_SHELL};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("reading config: {0}")]
    Read(#[from] std::io::Error),
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("validating config: {0}")]
    Resolve(#[from] ResolveError),
}

const CONFIG_ENV: &str = "WARDEN_CONFIG";
const CONFIG_DIR: &str = "warden";

/// Config path to load at launch: `$WARDEN_CONFIG` if set and non-empty, else
/// `~/.config/warden/config.toml`. Shared with curator and lector via config-core — the
/// set-but-empty fall-through this app already had is now the shared behaviour, and fixed the
/// other two.
pub fn config_path() -> PathBuf {
    config_core::resolve_config_path(CONFIG_ENV, CONFIG_DIR)
}

/// Load with the built-in [`DEFAULT_SHELL`] fallback. Convenience for tests; the app/CLI
/// call [`load_with`] to inject the user's detected login shell.
pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    load_with(path, DEFAULT_SHELL)
}

/// Read + parse + resolve, defaulting an unset `shell` to `default_shell` (the caller's
/// detected login shell). This is the path the app and the watcher use so hot-reload keeps
/// the same login-shell default as the initial load.
pub fn load_with(path: &Path, default_shell: &str) -> Result<Loaded, LoadError> {
    let text = std::fs::read_to_string(path)?;
    let raw = parse(&text)?;
    let (config, warnings) = resolve_with(raw, default_shell)?;
    Ok(Loaded { config, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_cfg(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn loads_valid_file() {
        let (_d, path) = write_cfg(
            r##"
[[window]]
title = "work"
colour = "#0f8a8a"
  [[window.tab]]
  dir = "/tmp/alpha"
"##,
        );
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.config.windows[0].title, "work");
    }

    #[test]
    fn missing_file_is_read_error() {
        let err = load(Path::new("/no/such/warden.toml")).unwrap_err();
        assert!(matches!(err, LoadError::Read(_)));
    }

    #[test]
    fn invalid_toml_is_parse_error() {
        let (_d, path) = write_cfg("this = = bad");
        assert!(matches!(load(&path).unwrap_err(), LoadError::Parse(_)));
    }

    #[test]
    fn invalid_colour_is_resolve_error() {
        let (_d, path) = write_cfg("[[window]]\ntitle=\"x\"\ncolour=\"nope\"\n");
        assert!(matches!(load(&path).unwrap_err(), LoadError::Resolve(_)));
    }

    #[test]
    fn config_path_respects_env() {
        std::env::set_var("WARDEN_CONFIG", "/custom/warden.toml");
        let result = config_path();
        std::env::remove_var("WARDEN_CONFIG");
        assert_eq!(result, PathBuf::from("/custom/warden.toml"));
    }
}
