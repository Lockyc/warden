//! Which Rex session each pane lives in: pane key → {server, session id}, persisted so a pane
//! reattaches to its session after warden restarts.
//!
//! One JSON file beside `session.rs`'s, scoped per config file by
//! `shell_core::config_scoped_filename`, written atomically on every change.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Filename stem handed to `shell_core::config_scoped_filename`.
pub const STEM: &str = "rex-sessions";

/// Where one pane's session lives. `server` is the config's `server` value (`"local"` or a host
/// label), never a resolved endpoint, so a host that changes address keeps its panes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneRecord {
    pub server: String,
    pub session_id: String,
}

/// Every pane's record plus where they persist. `path: None` (the `Default`) keeps records in
/// memory only.
#[derive(Debug, Default)]
pub struct RexStore {
    path: Option<PathBuf>,
    records: BTreeMap<String, PaneRecord>,
}

impl RexStore {
    /// Load from `path`. A missing or unreadable file is an empty store: the cost of losing it is
    /// each pane creating a fresh session, and the old ones showing as unclaimed.
    pub fn load(path: PathBuf) -> Self {
        let records = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        RexStore {
            path: Some(path),
            records,
        }
    }

    pub fn get(&self, pane_key: &str) -> Option<&PaneRecord> {
        self.records.get(pane_key)
    }

    /// Set `pane_key`'s record (`None` forgets it) and persist, writing only on a change.
    pub fn set(&mut self, pane_key: &str, record: Option<PaneRecord>) {
        let changed = match record {
            Some(r) => self.records.insert(pane_key.to_string(), r.clone()) != Some(r),
            None => self.records.remove(pane_key).is_some(),
        };
        if changed {
            if let Some(path) = &self.path {
                if let Err(e) = crate::session::write_atomic(path, &self.records) {
                    eprintln!("warden: couldn't save Rex sessions to {path:?}: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(server: &str, id: &str) -> PaneRecord {
        PaneRecord {
            server: server.into(),
            session_id: id.into(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("rex.json");
        let mut s = RexStore::load(path.clone());
        assert!(s.get("t").is_none(), "missing file is an empty store");
        s.set("t", Some(rec("local", "session:a")));
        assert_eq!(
            RexStore::load(path.clone()).get("t"),
            Some(&rec("local", "session:a"))
        );
        s.set("t", None);
        assert!(RexStore::load(path).get("t").is_none(), "None forgets");
    }

    #[test]
    fn garbage_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rex.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(RexStore::load(path).get("t").is_none());
    }
}
