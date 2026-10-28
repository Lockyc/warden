//! Agent records: what warden keeps about an agent tab that Rex can't. Rex owns which agents
//! exist (the windows of a pane's session) and their labels; a record adds the group, order,
//! resume command, and the asleep and closed states that outlive the window.
//!
//! One JSON file beside `store.rs`'s, scoped per config file by
//! `shell_core::config_scoped_filename`, written atomically on every change. `reconcile` is the
//! pure merge of a project's records with its session's current windows.

// The agent tab list is the first caller of these; until then only the tests read them.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::windows::AgentWindow;

/// Filename stem handed to `shell_core::config_scoped_filename`.
pub const STEM: &str = "agent-tabs";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RecordState {
    /// Backed by a window of the project's session.
    Live,
    /// The session is gone; waking the tab types `resume` into a fresh one.
    Asleep,
    /// The window was closed; kept so it can be reopened from `resume`.
    Closed,
}

/// One agent tab. `project` is the project tab's key (`Tab::key`). `selected` is the project's
/// last-selected record; at most one per project, enforced by the lifecycle, not `upsert`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub project: String,
    pub name: String,
    pub group: Option<String>,
    pub position: u32,
    pub resume: Option<String>,
    pub state: RecordState,
    pub window_id: Option<String>,
    pub block_id: Option<String>,
    /// Unix seconds, stamped by the caller of `reconcile`.
    pub closed_at: Option<u64>,
    pub selected: bool,
}

/// A fresh record id: wall-clock nanoseconds plus a process-wide counter, in hex. Unique within
/// a process by the counter and across runs by the clock; ids are opaque and never parsed.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The resume shapes a report may take: the fixed words (program and flags) before the one id.
/// A closed table because a forged report must not choose the program or its flags; a new
/// harness, or a new syntax for one, is one row.
const RESUME_SHAPES: &[&[&str]] = &[&["claude", "--resume"], &["codex", "resume"]];

/// Whether a reported resume command is safe to store: exactly a row of `RESUME_SHAPES`, then
/// one non-empty id of `[A-Za-z0-9-]`, joined by single spaces, nothing else.
fn resume_is_safe(s: &str) -> bool {
    let words: Vec<&str> = s.split(' ').collect();
    let Some((id, fixed)) = words.split_last() else {
        return false;
    };
    !id.is_empty()
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && RESUME_SHAPES.contains(&fixed)
}

/// Every record plus where they persist. `path: None` (the `Default`) keeps records in memory
/// only.
#[derive(Debug, Default)]
pub struct AgentStore {
    path: Option<PathBuf>,
    records: Vec<AgentRecord>,
}

impl AgentStore {
    /// Load from `path`. A missing or unreadable file is an empty store. A `resume` that fails
    /// the report check is cleared (the record stays), so a tampered file can't bypass the gate.
    pub fn load(path: PathBuf) -> Self {
        let mut records: Vec<AgentRecord> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        for r in &mut records {
            if r.resume.as_deref().is_some_and(|c| !resume_is_safe(c)) {
                r.resume = None;
            }
        }
        AgentStore {
            path: Some(path),
            records,
        }
    }

    /// `project`'s records in position order.
    pub fn records(&self, project: &str) -> Vec<AgentRecord> {
        let mut v: Vec<AgentRecord> = self
            .records
            .iter()
            .filter(|r| r.project == project)
            .cloned()
            .collect();
        v.sort_by_key(|r| r.position);
        v
    }

    /// Insert `record`, or replace the one with its id, and persist.
    pub fn upsert(&mut self, record: AgentRecord) {
        match self.records.iter_mut().find(|r| r.id == record.id) {
            Some(slot) => *slot = record,
            None => self.records.push(record),
        }
        self.save();
    }

    /// Forget the record with `id` and persist.
    pub fn remove(&mut self, id: &str) {
        let before = self.records.len();
        self.records.retain(|r| r.id != id);
        if self.records.len() != before {
            self.save();
        }
    }

    /// Store an agent's reported resume command on the record owning `block_id`. The one place a
    /// report enters the records; an unsafe report is refused. Returns whether a record took it.
    pub fn report_resume(&mut self, block_id: &str, resume: &str) -> bool {
        if !resume_is_safe(resume) {
            return false;
        }
        let Some(rec) = self
            .records
            .iter_mut()
            .find(|r| r.block_id.as_deref() == Some(block_id))
        else {
            return false;
        };
        if rec.resume.as_deref() != Some(resume) {
            rec.resume = Some(resume.to_string());
            self.save();
        }
        true
    }

    fn save(&self) {
        if let Some(path) = &self.path {
            if let Err(e) = crate::session::write_atomic(path, &self.records) {
                eprintln!("warden: couldn't save agent tabs to {path:?}: {e}");
            }
        }
    }
}

/// Merge `project`'s `records` with its session's current `windows`. Live records follow their
/// window (label as name, block); a vanished window closes a resumable record (stamped `now`)
/// and drops the rest; an unknown window is appended live, ungrouped, last. A dead session
/// sleeps resumable live records and drops the rest, ignoring `windows`. Asleep and Closed pass
/// through unchanged.
pub fn reconcile(
    project: &str,
    records: &[AgentRecord],
    windows: &[AgentWindow],
    session_alive: bool,
    now: u64,
) -> Vec<AgentRecord> {
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        if r.state != RecordState::Live {
            out.push(r.clone());
            continue;
        }
        let win = if session_alive {
            windows
                .iter()
                .find(|w| r.window_id.as_deref() == Some(&w.window_id))
        } else {
            None
        };
        let mut r = r.clone();
        match win {
            Some(w) => {
                r.name = w.label.clone();
                r.block_id = w.block_id.clone();
            }
            None if r.resume.is_none() => continue,
            None => {
                r.window_id = None;
                r.block_id = None;
                if session_alive {
                    r.state = RecordState::Closed;
                    r.closed_at = Some(now);
                } else {
                    r.state = RecordState::Asleep;
                }
            }
        }
        out.push(r);
    }
    if session_alive {
        let mut next = records
            .iter()
            .filter(|r| r.project == project)
            .map(|r| r.position + 1)
            .max()
            .unwrap_or(0);
        for w in windows {
            if out
                .iter()
                .any(|r| r.window_id.as_deref() == Some(&w.window_id))
            {
                continue;
            }
            out.push(AgentRecord {
                id: new_id(),
                project: project.to_string(),
                name: w.label.clone(),
                group: None,
                position: next,
                resume: None,
                state: RecordState::Live,
                window_id: Some(w.window_id.clone()),
                block_id: w.block_id.clone(),
                closed_at: None,
                selected: false,
            });
            next += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        id: &str,
        state: RecordState,
        window: Option<&str>,
        resume: Option<&str>,
    ) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            project: "p".into(),
            name: id.into(),
            group: None,
            position: 0,
            resume: resume.map(Into::into),
            state,
            window_id: window.map(Into::into),
            block_id: window.map(|w| format!("b-{w}")),
            closed_at: None,
            selected: false,
        }
    }

    fn win(id: &str, label: &str) -> AgentWindow {
        AgentWindow {
            window_id: id.into(),
            block_id: Some(format!("b-{id}")),
            label: label.into(),
        }
    }

    #[test]
    fn vanished_window_with_resume_goes_closed() {
        let r = rec(
            "a",
            RecordState::Live,
            Some("w1"),
            Some("claude --resume x"),
        );
        let out = reconcile("p", &[r], &[], true, 99);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, RecordState::Closed);
        assert_eq!(out[0].closed_at, Some(99));
        assert_eq!(out[0].window_id, None);
    }

    #[test]
    fn vanished_window_without_resume_drops() {
        let r = rec("a", RecordState::Live, Some("w1"), None);
        assert!(reconcile("p", &[r], &[], true, 1).is_empty());
    }

    #[test]
    fn unknown_window_appended_ungrouped() {
        let mut a = rec("a", RecordState::Live, Some("w1"), None);
        a.position = 4;
        let out = reconcile(
            "p",
            &[a],
            &[win("w1", "renamed"), win("w2", "new")],
            true,
            1,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "renamed", "Rex owns the label");
        let n = &out[1];
        assert_eq!((n.name.as_str(), n.position), ("new", 5));
        assert_eq!(n.state, RecordState::Live);
        assert_eq!((n.group.clone(), n.resume.clone()), (None, None));
        assert_eq!(n.window_id.as_deref(), Some("w2"));
        assert_eq!(n.block_id.as_deref(), Some("b-w2"));
        assert_eq!(n.project, "p");
    }

    #[test]
    fn dead_session_sleeps_resumable_and_drops_rest() {
        let keep = rec("a", RecordState::Live, Some("w1"), Some("claude"));
        let lose = rec("b", RecordState::Live, Some("w2"), None);
        let out = reconcile(
            "p",
            &[keep, lose],
            &[win("w1", "x"), win("w3", "y")],
            false,
            1,
        );
        assert_eq!(out.len(), 1, "no new records from a dead session");
        assert_eq!(out[0].state, RecordState::Asleep);
        assert_eq!(out[0].window_id, None);
        assert_eq!(out[0].block_id, None);
    }

    #[test]
    fn asleep_and_closed_untouched() {
        let mut closed = rec("c", RecordState::Closed, None, Some("claude"));
        closed.closed_at = Some(5);
        let asleep = rec("s", RecordState::Asleep, None, None);
        let input = [closed, asleep];
        for alive in [true, false] {
            assert_eq!(reconcile("p", &input, &[], alive, 9), input);
        }
    }

    #[test]
    fn report_resume_sets_record_by_block() {
        let mut s = AgentStore::default();
        s.upsert(rec("a", RecordState::Live, Some("w1"), None));
        assert!(s.report_resume("b-w1", "claude --resume abc123"));
        assert_eq!(
            s.records("p")[0].resume.as_deref(),
            Some("claude --resume abc123")
        );
        assert!(!s.report_resume("nope", "claude"), "no such block");
    }

    #[test]
    fn report_resume_refuses_injection() {
        let mut s = AgentStore::default();
        s.upsert(rec("a", RecordState::Live, Some("w1"), None));
        for bad in [
            "sh -c 'id'",
            "eval 'id'",
            ". /tmp/x",
            "bash -c id",
            "claude --resume 'x y'",
            "claude --resume x; id",
            "claude --resume $(id)",
            "claude --resume\nid",
            "claude",
            "claude  --resume x",
            "claude --mcp-config /tmp/x.json --resume x",
            "claude --resume x --dangerously-skip-permissions",
            "claude --settings /tmp/s --resume x",
            "codex -c k=v resume x",
            "claude --resume x/y",
            "codex --resume x",
        ] {
            assert!(!s.report_resume("b-w1", bad), "{bad:?}");
        }
        assert_eq!(s.records("p")[0].resume, None);
        assert!(s.report_resume(
            "b-w1",
            "claude --resume 6f1c2d3e-aaaa-bbbb-cccc-123456789abc"
        ));
        assert!(s.report_resume("b-w1", "codex resume abc123"));
    }

    #[test]
    fn load_clears_tampered_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let mut s = AgentStore::load(path.clone());
        s.upsert(rec("a", RecordState::Asleep, None, Some("sh -c 'id'")));
        s.upsert(rec(
            "b",
            RecordState::Asleep,
            None,
            Some("claude --resume x"),
        ));
        let loaded = AgentStore::load(path);
        let r = loaded.records("p");
        assert_eq!(r.len(), 2, "records are kept");
        let by = |id: &str| r.iter().find(|x| x.id == id).unwrap().resume.clone();
        assert_eq!(by("a"), None);
        assert_eq!(by("b").as_deref(), Some("claude --resume x"));
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn store_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("agents.json");
        let mut s = AgentStore::load(path.clone());
        assert!(s.records("p").is_empty(), "missing file is an empty store");
        let mut b = rec("b", RecordState::Asleep, None, Some("claude --resume x"));
        b.position = 1;
        s.upsert(b.clone());
        s.upsert(rec("a", RecordState::Live, Some("w1"), None));
        let loaded = AgentStore::load(path.clone());
        assert_eq!(loaded.records("p").len(), 2);
        assert_eq!(loaded.records("p")[1], b, "position order");
        assert!(!path.with_extension("json.tmp").exists());
        s.remove("a");
        assert_eq!(AgentStore::load(path.clone()).records("p"), vec![b]);
        std::fs::write(&path, b"not json").unwrap();
        assert!(AgentStore::load(path).records("p").is_empty());
    }
}
