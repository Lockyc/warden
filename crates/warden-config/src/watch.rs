use crate::load::{load, LoadError, Loaded};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::path::PathBuf;

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
    /// intentionally left to the consumer (deferred to Plan 2), which owns the reload UX.
    /// Atomic-save editors (e.g., vim, VSCode) are unaffected.
    pub fn new(
        path: PathBuf,
        on_change: impl Fn(Result<Loaded, LoadError>) + Send + 'static,
    ) -> notify::Result<Watcher> {
        // `parent()` returns Some("") for a bare relative filename (e.g. "config.toml"),
        // and watching "" errors. Treat an empty parent the same as None → watch the cwd.
        let watch_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let target = path.clone();
        // Capture the file name separately so the closure can match by name rather than full path.
        // macOS FSEvents reports canonical /private/var/... paths while tempfile (and callers) may
        // hold /var/... symlink paths, so exact-path equality fails. Since we watch a single
        // NonRecursive directory, matching by file name is sufficient and canonicalization-robust.
        let want_name = path.file_name().map(|n| n.to_owned());
        let mut inner = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                // Match by file name rather than full path for canonicalization-robustness:
                // macOS FSEvents reports canonical /private/var/... paths while callers may
                // hold /var/... symlink paths, so exact-path equality fails. Fire on any
                // event for the target file — atomic-save editors (e.g. vim, VSCode) may
                // rename a temp file over the target, which surfaces as Create, not Modify.
                if event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == want_name.as_deref())
                {
                    on_change(load(&target));
                }
            }
        })?;
        inner.watch(&watch_dir, RecursiveMode::NonRecursive)?;
        Ok(Watcher { _inner: inner })
    }
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
        write(&path, "[[window]]\nname=\"a\"\ncolour=\"#000000\"\n");

        let (tx, rx) = mpsc::channel();
        let _w = Watcher::new(path.clone(), move |res| {
            let _ = tx.send(res.map(|l| l.config.windows[0].name.clone()));
        })
        .unwrap();

        // Give the watcher a moment to register, then modify.
        std::thread::sleep(Duration::from_millis(200));
        write(&path, "[[window]]\nname=\"b\"\ncolour=\"#000000\"\n");

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
}
