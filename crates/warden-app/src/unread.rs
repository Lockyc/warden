//! Unread bell / desktop-notification badges (`notify::raise`), kept until the tab is seen so a
//! restart keeps them. Keyed by a window's launch label (`WindowState::session_key`, the key
//! `session.rs` uses) and the tab's id; a popped-out tab's badge lives under its origin's.
//!
//! One JSON file beside the session store, scoped per config file, written atomically on every
//! change. No AppKit/Tauri: unit-tests against temp dirs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Filename stem handed to `shell_core::config_scoped_filename`.
pub const STEM: &str = "unread";

/// Every window's unread tab ids, keyed by launch label, plus where they persist. `path: None`
/// (the `Default`) keeps them in memory only.
#[derive(Debug, Default)]
pub struct UnreadStore {
    path: Option<PathBuf>,
    tabs: BTreeMap<String, BTreeSet<String>>,
}

impl UnreadStore {
    /// Load from `path`. A missing or unreadable file is an empty store: the worst case is a
    /// badge lost, as it would be without this store.
    pub fn load(path: PathBuf) -> Self {
        let tabs = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        UnreadStore {
            path: Some(path),
            tabs,
        }
    }

    pub fn has(&self, key: &str, tab: &str) -> bool {
        self.tabs.get(key).is_some_and(|t| t.contains(tab))
    }

    /// Mark `tab` of the window keyed `key` unread.
    pub fn add(&mut self, key: &str, tab: &str) {
        if self
            .tabs
            .entry(key.to_owned())
            .or_default()
            .insert(tab.to_owned())
        {
            self.save();
        }
    }

    /// Mark `tab` of the window keyed `key` seen.
    pub fn clear(&mut self, key: &str, tab: &str) {
        let Some(set) = self.tabs.get_mut(key) else {
            return;
        };
        if set.remove(tab) {
            if set.is_empty() {
                self.tabs.remove(key);
            }
            self.save();
        }
    }

    /// Move `from`'s badges to `to`: a window renamed in place now launches under `to`.
    pub fn rekey(&mut self, from: &str, to: &str) {
        if let Some(set) = self.tabs.remove(from) {
            self.tabs.entry(to.to_owned()).or_default().extend(set);
            self.save();
        }
    }

    fn save(&self) {
        if let Some(path) = &self.path {
            if let Err(e) = crate::session::write_atomic(path, &self.tabs) {
                log::error!("couldn't save unread badges to {path:?}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_badge_survives_a_reload_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("u.json");
        let mut s = UnreadStore::load(path.clone());
        assert!(!s.has("w", "a"), "missing file is an empty store");
        s.add("w", "a");
        s.add("w", "b");
        let back = UnreadStore::load(path.clone());
        assert!(back.has("w", "a") && back.has("w", "b"));
        assert!(!back.has("other", "a"), "keyed per window");
        s.clear("w", "a");
        s.clear("w", "b");
        let back = UnreadStore::load(path.clone());
        assert!(!back.has("w", "a") && !back.has("w", "b"));
        assert_eq!(std::fs::read_to_string(path).unwrap().trim(), "{}");
    }

    #[test]
    fn rekey_moves_badges_to_the_renamed_windows_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.json");
        let mut s = UnreadStore::load(path.clone());
        s.add("Docs", "a");
        s.rekey("Docs", "Notes");
        let back = UnreadStore::load(path);
        assert!(!back.has("Docs", "a"));
        assert!(back.has("Notes", "a"));
    }

    #[test]
    fn unchanged_badges_do_not_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.json");
        let mut s = UnreadStore::load(path.clone());
        s.add("w", "a");
        std::fs::remove_file(&path).unwrap();
        s.add("w", "a");
        s.clear("w", "absent");
        s.clear("nowhere", "a");
        s.rekey("nowhere", "w");
        assert!(!path.exists(), "nothing changed → no write");
    }

    #[test]
    fn garbage_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(!UnreadStore::load(path).has("w", "a"));
    }
}
