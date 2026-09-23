//! Remembered tabs (`remember_tabs`): per window label, which tabs had a terminal standing and
//! which tab was active, so a window built later — next launch, or a reopen — comes back the way
//! it was left.
//!
//! One JSON file beside shell-core's geometry store, scoped per config file by
//! `shell_core::config_scoped_filename`. Written on every change to a window's loaded set or
//! active tab (never on quit — see `WindowManager::record_session`), so a crash loses nothing.
//! No AppKit/Tauri: unit-tests against temp dirs.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Filename stem handed to `shell_core::config_scoped_filename`.
pub const STEM: &str = "tab-session";

/// One window's remembered state. Ids are `TabSpec::id` (`Tab::key`: id-else-normalized-dir),
/// so a record survives a relabel and a discovered tab keeps its identity across rescans.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowRecord {
    /// Tabs with a terminal standing — live, or popped out (which restores docked).
    pub loaded: Vec<String>,
    pub active: Option<String>,
}

/// What a window restores from its record, filtered against the tabs it has now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Restore {
    pub loaded: HashSet<String>,
    pub active: Option<String>,
}

impl WindowRecord {
    /// Restrict this record to `tab_ids` (the window's current tabs): a tab dropped from the
    /// config, or a discovered project whose folder is gone, is skipped rather than failing.
    pub fn restore<'a>(&self, tab_ids: impl IntoIterator<Item = &'a str>) -> Restore {
        let present: HashSet<&str> = tab_ids.into_iter().collect();
        Restore {
            loaded: self
                .loaded
                .iter()
                .filter(|id| present.contains(id.as_str()))
                .cloned()
                .collect(),
            active: self
                .active
                .clone()
                .filter(|id| present.contains(id.as_str())),
        }
    }
}

/// Every window's record, keyed by Tauri label, plus where they persist. `path: None` (the
/// `Default`) keeps records in memory only — the shape tests and a failed path lookup get.
#[derive(Debug, Default)]
pub struct SessionStore {
    path: Option<PathBuf>,
    records: BTreeMap<String, WindowRecord>,
}

impl SessionStore {
    /// Load from `path`. A missing or unreadable file is an empty store, never an error — the
    /// worst case of losing it is a window opening the way it would without `remember_tabs`.
    pub fn load(path: PathBuf) -> Self {
        let records = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        SessionStore {
            path: Some(path),
            records,
        }
    }

    pub fn get(&self, label: &str) -> Option<&WindowRecord> {
        self.records.get(label)
    }

    /// Set `label`'s record (`None` forgets it) and persist, writing only when something
    /// changed. The write is atomic (temp file + rename) so a crash mid-write can't leave a
    /// truncated file behind.
    pub fn set(&mut self, label: &str, record: Option<WindowRecord>) {
        let changed = match record {
            Some(r) => self.records.insert(label.to_string(), r.clone()) != Some(r),
            None => self.records.remove(label).is_some(),
        };
        if changed {
            if let Some(path) = &self.path {
                if let Err(e) = write_atomic(path, &self.records) {
                    eprintln!("warden: couldn't save remembered tabs to {path:?}: {e}");
                }
            }
        }
    }
}

fn write_atomic(path: &Path, records: &BTreeMap<String, WindowRecord>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(loaded: &[&str], active: Option<&str>) -> WindowRecord {
        WindowRecord {
            loaded: loaded.iter().map(|s| s.to_string()).collect(),
            active: active.map(str::to_string),
        }
    }

    #[test]
    fn restore_drops_ids_the_window_no_longer_has() {
        let r = rec(&["a", "gone", "c"], Some("gone")).restore(["a", "b", "c"]);
        assert_eq!(
            r.loaded,
            ["a", "c"]
                .iter()
                .map(|s| s.to_string())
                .collect::<HashSet<_>>()
        );
        assert_eq!(
            r.active, None,
            "a vanished active tab falls back to the default"
        );
        let r = rec(&[], Some("b")).restore(["a", "b"]);
        assert_eq!(r.active.as_deref(), Some("b"));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".tab-session-x.json");
        let mut s = SessionStore::load(path.clone());
        assert!(s.get("w").is_none(), "missing file is an empty store");
        s.set("w", Some(rec(&["a", "b"], Some("b"))));
        let back = SessionStore::load(path.clone());
        assert_eq!(back.get("w"), Some(&rec(&["a", "b"], Some("b"))));
        s.set("w", None);
        assert!(SessionStore::load(path).get("w").is_none(), "None forgets");
    }

    #[test]
    fn garbage_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(SessionStore::load(path).get("w").is_none());
    }

    #[test]
    fn unchanged_record_does_not_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut s = SessionStore::load(path.clone());
        s.set("w", Some(rec(&["a"], None)));
        std::fs::remove_file(&path).unwrap();
        s.set("w", Some(rec(&["a"], None)));
        assert!(!path.exists(), "same record → no write");
        s.set("absent", None);
        assert!(!path.exists(), "forgetting nothing → no write");
    }
}
