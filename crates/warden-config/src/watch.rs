use crate::load::{load_with, LoadError, Loaded};
use crate::resolve::DEFAULT_SHELL;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub struct Watcher {
    _inner: RecommendedWatcher,
}

impl Watcher {
    /// Watch a config file for changes and invoke a callback on filesystem events.
    ///
    /// # Preconditions
    ///
    /// The parent directory of `path` must already exist. The `notify` crate's `watch()` returns
    /// an error if the directory is absent, so callers must ensure the config directory exists
    /// before constructing a `Watcher`. The watcher does not create or retry the directory.
    ///
    /// # Known Limitations
    ///
    /// The watcher invokes the callback for every filesystem event matching the config file name,
    /// with **no debounce or coalescing**. Editors that write in place (rather than atomic
    /// temp-file + rename) can therefore produce a transient `load()` parse error (a partial
    /// read mid-write) and/or multiple callbacks per save. Debouncing and coalescing are
    /// left to the consumer, which owns the reload UX (deferred — `docs/FOLLOWUPS.md`).
    /// Atomic-save editors (e.g., vim, VSCode) are unaffected.
    pub fn new(
        path: PathBuf,
        on_change: impl Fn(Result<Loaded, LoadError>) + Send + 'static,
    ) -> notify::Result<Watcher> {
        Watcher::with_default(path, DEFAULT_SHELL, on_change)
    }

    /// Like [`Watcher::new`], but each reload defaults an unset `shell` to `default_shell`
    /// (the caller's detected login shell) — so hot-reload keeps the same login-shell default
    /// as the app's initial load instead of falling back to [`DEFAULT_SHELL`].
    pub fn with_default(
        path: PathBuf,
        default_shell: impl Into<String>,
        on_change: impl Fn(Result<Loaded, LoadError>) + Send + 'static,
    ) -> notify::Result<Watcher> {
        let default_shell = default_shell.into();
        // A symlinked config (`config.toml -> ~/dotfiles/warden.toml`) is edited at its target,
        // so the events land in the target's directory under the target's name: watch both the
        // link's and the target's (dir, name). The link's pair still catches the link itself
        // being replaced.
        let mut watched = vec![dir_and_name(&path)];
        if path.is_symlink() {
            if let Ok(target) = std::fs::canonicalize(&path) {
                let pair = dir_and_name(&target);
                if !watched.contains(&pair) {
                    watched.push(pair);
                }
            }
        }
        let names: Vec<_> = watched.iter().filter_map(|(_, n)| n.clone()).collect();
        let target = path.clone();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                // Match by file name, not full path: macOS FSEvents reports canonical
                // /private/var/... paths while callers may hold /var/... symlink paths, so
                // exact-path equality fails, and each watch is a single NonRecursive directory.
                // Fire on any event kind — atomic-save editors (vim, VSCode) rename a temp file
                // over the target, which surfaces as Create, not Modify.
                if event
                    .paths
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| names.iter().any(|w| w == n)))
                {
                    on_change(load_with(&target, &default_shell));
                }
            }
        })?;
        for dir in watched
            .iter()
            .map(|(d, _)| d)
            .collect::<std::collections::BTreeSet<_>>()
        {
            inner.watch(dir, RecursiveMode::NonRecursive)?;
        }
        Ok(Watcher { _inner: inner })
    }
}

/// The directory to watch for `path` and the file name to match in it. `parent()` is
/// `Some("")` for a bare relative filename, and watching "" errors, so that maps to the cwd.
fn dir_and_name(path: &Path) -> (PathBuf, Option<OsString>) {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    (dir, path.file_name().map(|n| n.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::tempdir;

    fn write(path: &std::path::Path, body: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn fires_callback_on_save() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "[[window]]\ntitle=\"a\"\ncolour=\"#000000\"\n");

        let (tx, rx) = mpsc::channel();
        let _w = Watcher::new(path.clone(), move |res| {
            let _ = tx.send(res.map(|l| l.config.windows[0].title.clone()));
        })
        .unwrap();

        // Give the watcher a moment to register, then modify.
        std::thread::sleep(Duration::from_millis(200));
        write(&path, "[[window]]\ntitle=\"b\"\ncolour=\"#000000\"\n");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let got = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(v) => {
                    if v.as_deref().ok() == Some("b") {
                        break v;
                    }
                    // stale early event (e.g. the initial create) — keep draining
                }
                Err(_) => panic!("timed out waiting for callback with window 'b'"),
            }
        };
        assert_eq!(got.unwrap(), "b");
    }

    #[test]
    fn fires_callback_when_a_symlinked_config_is_edited_at_its_target() {
        let link_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        let target = target_dir.path().join("warden.toml");
        write(&target, "[[window]]\ntitle=\"a\"\ncolour=\"#000000\"\n");
        let link = link_dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (tx, rx) = mpsc::channel();
        let _w = Watcher::new(link, move |res| {
            let _ = tx.send(res.map(|l| l.config.windows[0].title.clone()));
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(200));
        write(&target, "[[window]]\ntitle=\"b\"\ncolour=\"#000000\"\n");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(v) if v.as_deref().ok() == Some("b") => break,
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for a reload of the symlink target"),
            }
        }
    }
}
