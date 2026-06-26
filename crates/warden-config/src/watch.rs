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
        let mut inner = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if event.paths.iter().any(|p| p == &target) {
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
