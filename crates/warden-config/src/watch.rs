use crate::load::{load, Loaded, LoadError};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::path::PathBuf;

pub struct Watcher {
    _inner: RecommendedWatcher,
}

impl Watcher {
    pub fn new(
        path: PathBuf,
        on_change: impl Fn(Result<Loaded, LoadError>) + Send + 'static,
    ) -> notify::Result<Watcher> {
        let watch_dir = path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let target = path.clone();
        // Capture the file name separately so the closure can match by name rather than full path.
        // macOS FSEvents reports canonical /private/var/... paths while tempfile (and callers) may
        // hold /var/... symlink paths, so exact-path equality fails. Since we watch a single
        // NonRecursive directory, matching by file name is sufficient and canonicalization-robust.
        let want_name = path.file_name().map(|n| n.to_owned());
        let mut inner = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                // Only react to data-modify events (not initial creates) and match by
                // file name rather than full path for canonicalization-robustness.
                if event.kind.is_modify()
                    && event.paths.iter().any(|p| p.file_name() == want_name.as_deref())
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
        write(&path, "[[profile]]\nname=\"a\"\ncolour=\"#000000\"\n");

        let (tx, rx) = mpsc::channel();
        let _w = Watcher::new(path.clone(), move |res| {
            let _ = tx.send(res.map(|l| l.config.profiles[0].name.clone()));
        })
        .unwrap();

        // Give the watcher a moment to register, then modify.
        std::thread::sleep(Duration::from_millis(200));
        write(&path, "[[profile]]\nname=\"b\"\ncolour=\"#000000\"\n");

        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got.unwrap(), "b");
    }
}
