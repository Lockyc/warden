//! Agent records: what warden keeps about an agent tab that Rex can't. Rex owns which agents
//! exist (the windows of a pane's session) and their labels; a record adds the group, order,
//! resume command, and the asleep and closed states that outlive the window.
//!
//! One JSON file beside `store.rs`'s, scoped per config file by
//! `shell_core::config_scoped_filename`, written atomically on every change. `reconcile` is the
//! pure merge of a project's records with its session's current windows. Folded groups persist
//! beside the records.

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
    /// Forks made from this tab; numbers the next fork's default name.
    #[serde(default)]
    pub forks_made: u32,
    /// The record this tab was forked from; places the parent's next fork below this one.
    #[serde(default)]
    pub forked_from: Option<String>,
    /// The user named the tab, so a generated summary never overwrites the name.
    #[serde(default)]
    pub user_named: bool,
    /// The conversation's last good done/now/next rows ([`super::summary`]). Cleared with
    /// `subject` when `resume` changes: a new id is a new conversation.
    #[serde(default)]
    pub summary: Option<warden_summary::StandRows>,
    /// The conversation's stable subject label, kept across refreshes and restarts.
    #[serde(default)]
    pub subject: Option<String>,
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
const RESUME_SHAPES: &[&[&str]] = &[CLAUDE_RESUME, &["codex", "resume"]];

/// Claude Code's row of `RESUME_SHAPES`: the one harness with a fork flag and a transcript warden
/// reads.
const CLAUDE_RESUME: &[&str] = &["claude", "--resume"];

/// Whether a reported resume command is safe to store: exactly a row of `RESUME_SHAPES`, then
/// one id of `[A-Za-z0-9][A-Za-z0-9-]*` (never a leading `-`, which the CLI would read as a
/// flag), joined by single spaces, nothing else.
fn resume_is_safe(s: &str) -> bool {
    let words: Vec<&str> = s.split(' ').collect();
    let Some((id, fixed)) = words.split_last() else {
        return false;
    };
    id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && RESUME_SHAPES.contains(&fixed)
}

/// The flag that turns a `RESUME_SHAPES` row into a fork of that conversation. A harness without
/// a row has no fork command, so its tab can't be forked; Codex has none.
const FORK_FLAGS: &[(&[&str], &str)] = &[(CLAUDE_RESUME, "--fork-session")];

/// The split of a safe resume command into its fixed words and trailing id; `None` for anything
/// `resume_is_safe` refuses.
fn split_resume(resume: &str) -> Option<(Vec<&str>, &str)> {
    if !resume_is_safe(resume) {
        return None;
    }
    let mut words: Vec<&str> = resume.split(' ').collect();
    let id = words.pop()?;
    Some((words, id))
}

/// The command that forks the conversation a stored resume command resumes. Derived from the
/// already-checked resume command, never reported, so a forged report can't choose a program, a
/// flag or a file. `None` for an unsafe command or a harness with no `FORK_FLAGS` row.
pub fn fork_command(resume: &str) -> Option<String> {
    let (fixed, _) = split_resume(resume)?;
    let (_, flag) = FORK_FLAGS.iter().find(|(shape, _)| *shape == fixed)?;
    Some(format!("{resume} {flag}"))
}

/// The conversation id a safe resume command ends with; it names the transcript file. `None` for
/// an unsafe command.
pub fn resume_id(resume: &str) -> Option<&str> {
    split_resume(resume).map(|(_, id)| id)
}

/// Whether a safe resume command resumes a Claude Code conversation.
pub fn is_claude(resume: &str) -> bool {
    split_resume(resume).is_some_and(|(fixed, _)| fixed == CLAUDE_RESUME)
}

/// A folded group of a project's list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Fold {
    project: String,
    group: String,
}

/// The file's shape.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    records: Vec<AgentRecord>,
    #[serde(default)]
    folded: Vec<Fold>,
}

/// Every record and folded group, plus where they persist. `path: None` (the `Default`) keeps
/// them in memory only.
#[derive(Debug, Default)]
pub struct AgentStore {
    path: Option<PathBuf>,
    records: Vec<AgentRecord>,
    folded: Vec<Fold>,
}

impl AgentStore {
    /// Load from `path`. A missing or unreadable file is an empty store. A `resume` must pass the
    /// report check, so a tampered file can't bypass the gate: a live record failing it keeps
    /// running without one, and an asleep or closed record, which only `resume` can wake, drops.
    pub fn load(path: PathBuf) -> Self {
        let Saved {
            mut records,
            folded,
        } = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        for r in &mut records {
            if r.resume.as_deref().is_some_and(|c| !resume_is_safe(c)) {
                r.resume = None;
            }
        }
        records.retain(|r| r.state == RecordState::Live || r.resume.is_some());
        AgentStore {
            path: Some(path),
            records,
            folded,
        }
    }

    /// Make `id` `project`'s one selected record. False (nothing changes) for a record that isn't
    /// `project`'s or is closed.
    pub fn select(&mut self, project: &str, id: &str) -> bool {
        let ok = self
            .records
            .iter()
            .any(|r| r.id == id && r.project == project && r.state != RecordState::Closed);
        if ok {
            for r in self.records.iter_mut().filter(|r| r.project == project) {
                r.selected = r.id == id;
            }
            self.save();
        }
        ok
    }

    /// Move `project`'s record `id` into `group` (trimmed; blank or `None` is ungrouped) at
    /// `index` among that group's other rows (its records not closed, as the list shows them), or
    /// last; a group no row holds yet goes after the others. Closed records keep their place.
    /// Positions are renumbered in list order ([`in_list_order`]). False for an unknown record.
    pub fn set_group(
        &mut self,
        project: &str,
        id: &str,
        group: Option<&str>,
        index: Option<usize>,
    ) -> bool {
        let mut list = in_list_order(&self.records(project));
        let Some(at) = list.iter().position(|r| r.id == id) else {
            return false;
        };
        let mut moved = list.remove(at);
        moved.group = group
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(Into::into);
        let members: Vec<usize> = (0..list.len())
            .filter(|&i| list[i].group == moved.group && list[i].state != RecordState::Closed)
            .collect();
        let to = match (members.get(index.unwrap_or(usize::MAX)), members.last()) {
            (Some(&i), _) => i,
            (None, Some(&last)) => last + 1,
            // The first ungrouped record tops the list; a new group follows every other.
            (None, None) if moved.group.is_none() => 0,
            (None, None) => list.len(),
        };
        list.insert(to, moved);
        for (i, r) in list.iter_mut().enumerate() {
            r.position = i as u32;
        }
        self.set_project(project, list);
        true
    }

    /// Fold or unfold `project`'s `group`.
    pub fn set_folded(&mut self, project: &str, group: &str, folded: bool) {
        let fold = Fold {
            project: project.into(),
            group: group.into(),
        };
        let had = self.folded.contains(&fold);
        if had == folded {
            return;
        }
        if folded {
            self.folded.push(fold);
        } else {
            self.folded.retain(|f| *f != fold);
        }
        self.save();
    }

    /// `project`'s folded groups.
    pub fn folded(&self, project: &str) -> Vec<String> {
        self.folded
            .iter()
            .filter(|f| f.project == project)
            .map(|f| f.group.clone())
            .collect()
    }

    /// Forget `project`'s closed record `id`, or every closed record of it when `None`. Returns
    /// whether one went.
    pub fn clear_closed(&mut self, project: &str, id: Option<&str>) -> bool {
        let before = self.records.len();
        self.records.retain(|r| {
            !(r.project == project
                && r.state == RecordState::Closed
                && id.is_none_or(|id| r.id == id))
        });
        let cleared = self.records.len() != before;
        if cleared {
            self.save();
        }
        cleared
    }

    /// Forget every record and fold of `project`. Returns whether a record went.
    pub fn remove_project(&mut self, project: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.project != project);
        self.folded.retain(|f| f.project != project);
        let removed = self.records.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// The project whose record shows `block_id`.
    pub fn project_of_block(&self, block_id: &str) -> Option<String> {
        self.records
            .iter()
            .find(|r| r.block_id.as_deref() == Some(block_id))
            .map(|r| r.project.clone())
    }

    /// The project record `id` belongs to.
    pub fn project_of_record(&self, id: &str) -> Option<String> {
        self.records
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.project.clone())
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

    /// The record with `id`.
    pub fn record(&self, id: &str) -> Option<AgentRecord> {
        self.records.iter().find(|r| r.id == id).cloned()
    }

    /// The record showing `block_id`.
    pub fn by_block(&self, block_id: &str) -> Option<AgentRecord> {
        self.records
            .iter()
            .find(|r| r.block_id.as_deref() == Some(block_id))
            .cloned()
    }

    /// Every live record, of every project.
    pub fn live(&self) -> Vec<AgentRecord> {
        self.records
            .iter()
            .filter(|r| r.state == RecordState::Live)
            .cloned()
            .collect()
    }

    /// Insert `record`, or replace the one with its id, and persist.
    pub fn upsert(&mut self, record: AgentRecord) {
        match self.records.iter_mut().find(|r| r.id == record.id) {
            Some(slot) => *slot = record,
            None => self.records.push(record),
        }
        self.save();
    }

    /// Replace `project`'s records with `records` (a `reconcile` result) and persist.
    pub fn set_project(&mut self, project: &str, records: Vec<AgentRecord>) {
        self.records.retain(|r| r.project != project);
        self.records.extend(records);
        self.save();
    }

    /// Write back records a caller changed from `before` (what it read) to `after`, keeping every
    /// write made in between: only the fields the caller changed are applied, a record it added
    /// is inserted, and one it dropped is removed unless it changed in the meantime. Records it
    /// never read are untouched. Persists.
    pub fn merge(&mut self, before: &[AgentRecord], after: Vec<AgentRecord>) {
        let was = |id: &str| before.iter().find(|r| r.id == id);
        for b in before {
            let dropped = !after.iter().any(|a| a.id == b.id);
            if dropped {
                self.records.retain(|r| r.id != b.id || r != b);
            }
        }
        for a in after {
            match (was(&a.id), self.records.iter_mut().find(|r| r.id == a.id)) {
                (Some(b), Some(cur)) => merge_fields(cur, b, &a),
                (Some(_), None) => {} // removed meanwhile
                (None, Some(cur)) => *cur = a,
                (None, None) => self.records.push(a),
            }
        }
        self.save();
    }

    /// Forget the record with `id` and persist.
    #[cfg(test)]
    pub fn remove(&mut self, id: &str) {
        let before = self.records.len();
        self.records.retain(|r| r.id != id);
        if self.records.len() != before {
            self.save();
        }
    }

    /// Store an agent's reported resume command on the record owning `block_id`. The one place a
    /// report enters the records; an unsafe report is refused. A different command is a different
    /// conversation, so it clears the record's summary and subject. Returns whether a record took
    /// it.
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
            rec.summary = None;
            rec.subject = None;
            self.save();
        }
        true
    }

    fn save(&self) {
        if let Some(path) = &self.path {
            let saved = Saved {
                records: self.records.clone(),
                folded: self.folded.clone(),
            };
            if let Err(e) = crate::session::write_atomic(path, &saved) {
                eprintln!("warden: couldn't save agent tabs to {path:?}: {e}");
            }
        }
    }
}

/// Set on `current` each field `after` changed from `before`; the rest keep `current`'s value.
fn merge_fields(current: &mut AgentRecord, before: &AgentRecord, after: &AgentRecord) {
    macro_rules! changed {
        ($($f:ident),*) => {$(
            if after.$f != before.$f {
                current.$f = after.$f.clone();
            }
        )*};
    }
    changed!(
        project,
        name,
        group,
        position,
        resume,
        state,
        window_id,
        block_id,
        closed_at,
        selected,
        forks_made,
        forked_from,
        user_named,
        summary,
        subject
    );
}

/// `records` (one project's) in list order: ungrouped above the groups, groups ordered by their
/// first record's position, each by position.
pub fn in_list_order(records: &[AgentRecord]) -> Vec<AgentRecord> {
    let first = |g: &str| {
        records
            .iter()
            .filter(|r| r.group.as_deref() == Some(g))
            .map(|r| r.position)
            .min()
    };
    let key = |r: &AgentRecord| {
        let group = r.group.clone().map(|g| (first(&g), g));
        (group, r.position)
    };
    let mut v = records.to_vec();
    v.sort_by_cached_key(key);
    v
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
                forks_made: 0,
                forked_from: None,
                user_named: false,
                summary: None,
                subject: None,
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
            forks_made: 0,
            forked_from: None,
            user_named: false,
            summary: None,
            subject: None,
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
    fn fork_command_for_claude_appends_fork_session() {
        assert_eq!(
            fork_command("claude --resume abc-123").as_deref(),
            Some("claude --resume abc-123 --fork-session")
        );
        assert_eq!(resume_id("claude --resume abc-123"), Some("abc-123"));
        assert_eq!(fork_command("claude --resume -x"), None);
        assert_eq!(resume_id("claude --resume a; rm"), None);
    }

    #[test]
    fn no_fork_for_codex() {
        assert_eq!(fork_command("codex resume abc"), None);
        assert_eq!(resume_id("codex resume abc"), Some("abc"));
    }

    #[test]
    fn old_store_without_new_fields_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::write(
            &path,
            r#"{"records":[{"id":"a","project":"p","name":"a","group":null,"position":0,
            "resume":"claude --resume x","state":"Asleep","window_id":null,"block_id":null,
            "closed_at":null,"selected":false}]}"#,
        )
        .unwrap();
        let s = AgentStore::load(path);
        let r = &s.records[0];
        assert_eq!((r.forks_made, r.user_named), (0, false));
    }

    #[test]
    fn resume_change_clears_summary_and_subject() {
        let mut a = rec(
            "a",
            RecordState::Live,
            Some("w1"),
            Some("claude --resume x"),
        );
        a.summary = Some(warden_summary::StandRows {
            now: Some("reading".into()),
            ..Default::default()
        });
        a.subject = Some("auth flow".into());
        let mut s = AgentStore::default();
        s.upsert(a);

        assert!(s.report_resume("b-w1", "claude --resume x"));
        let kept = s.record("a").unwrap();
        assert!(
            kept.summary.is_some() && kept.subject.is_some(),
            "the same conversation keeps its summary"
        );

        assert!(s.report_resume("b-w1", "claude --resume y"));
        let cleared = s.record("a").unwrap();
        assert_eq!(cleared.resume.as_deref(), Some("claude --resume y"));
        assert_eq!((cleared.summary, cleared.subject), (None, None));
    }

    #[test]
    fn merge_carries_summary_and_subject() {
        let before = rec("a", RecordState::Live, Some("w1"), None);
        let mut after = before.clone();
        after.subject = Some("s".into());
        after.summary = Some(warden_summary::StandRows::default());
        let mut current = before.clone();
        merge_fields(&mut current, &before, &after);
        assert_eq!(current.subject.as_deref(), Some("s"));
        assert!(current.summary.is_some());
    }

    #[test]
    fn only_claude_resumes_are_claude() {
        assert!(is_claude("claude --resume abc"));
        assert!(!is_claude("codex resume abc"));
        assert!(!is_claude("claude --resume -x"));
    }

    #[test]
    fn merge_carries_forks_made_and_user_named() {
        let before = rec("a", RecordState::Live, Some("w1"), None);
        let mut after = before.clone();
        after.forks_made = 2;
        after.user_named = true;
        let mut current = before.clone();
        merge_fields(&mut current, &before, &after);
        assert_eq!((current.forks_made, current.user_named), (2, true));
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
            "claude --resume --dangerously-skip-permissions",
            "claude --resume -p",
            "codex resume --dangerously-bypass-approvals-and-sandbox",
            "codex resume --yolo",
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
        s.upsert(rec("c", RecordState::Closed, None, Some("sh -c 'id'")));
        s.upsert(rec("d", RecordState::Live, Some("w4"), Some("sh -c 'id'")));
        let loaded = AgentStore::load(path);
        let r = loaded.records("p");
        let by = |id: &str| r.iter().find(|x| x.id == id).map(|x| x.resume.clone());
        assert_eq!(by("a"), None, "asleep with nothing to wake it: dropped");
        assert_eq!(by("c"), None, "closed with nothing to reopen it: dropped");
        assert_eq!(by("b"), Some(Some("claude --resume x".into())));
        assert_eq!(by("d"), Some(None), "live: kept, without the command");
    }

    #[test]
    fn merge_keeps_writes_made_between_read_and_write() {
        let mut s = AgentStore::default();
        s.upsert(rec("a", RecordState::Live, Some("w1"), None));
        s.upsert(rec("b", RecordState::Live, Some("w2"), None));
        s.upsert(rec("c", RecordState::Live, Some("w3"), None));
        let before = s.records("p");
        // The caller drops `b` and `c` and renames `a`…
        let mut a = before[0].clone();
        a.name = "renamed".into();
        // …while `a` and `c` report a resume.
        assert!(s.report_resume("b-w1", "claude --resume aaa"));
        assert!(s.report_resume("b-w3", "claude --resume ccc"));
        s.merge(&before, vec![a]);
        let r = s.records("p");
        let ids: Vec<&str> = r.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"], "c changed meanwhile, so it stays");
        assert_eq!(r[0].name, "renamed");
        assert_eq!(r[0].resume.as_deref(), Some("claude --resume aaa"));
    }

    fn ids(s: &AgentStore) -> Vec<String> {
        s.records("p").into_iter().map(|r| r.id).collect()
    }

    fn three() -> AgentStore {
        let mut s = AgentStore::default();
        for (i, id) in ["a", "b", "c"].into_iter().enumerate() {
            let mut r = rec(id, RecordState::Live, Some(id), None);
            r.position = i as u32;
            s.upsert(r);
        }
        s
    }

    #[test]
    fn grouping_keeps_ungrouped_above_groups_and_renumbers() {
        let mut s = three();
        assert!(s.set_group("p", "a", Some("work"), None));
        assert_eq!(ids(&s), ["b", "c", "a"], "a new group goes last");
        assert!(s.set_group("p", "c", Some(" work "), Some(0)));
        assert_eq!(ids(&s), ["b", "c", "a"], "first in its group");
        assert_eq!(s.records("p")[1].group.as_deref(), Some("work"));
        assert!(s.set_group("p", "a", None, Some(0)));
        assert_eq!(ids(&s), ["a", "b", "c"], "ungrouped, at the top");
        assert!(s.set_group("p", "b", Some(""), None), "blank clears");
        assert_eq!(ids(&s), ["a", "b", "c"]);
        let positions: Vec<u32> = s.records("p").iter().map(|r| r.position).collect();
        assert_eq!(positions, [0, 1, 2]);
        assert!(!s.set_group("p", "nope", None, None));
    }

    #[test]
    fn grouping_indexes_rows_not_closed_records() {
        let mut s = AgentStore::default();
        for (i, (id, state)) in [
            ("x", RecordState::Closed),
            ("y", RecordState::Live),
            ("z", RecordState::Live),
            ("m", RecordState::Live),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = rec(id, state, None, None);
            r.position = i as u32;
            r.group = (id != "m").then(|| "g".to_string());
            s.upsert(r);
        }
        // The list shows g as [y, z]; dropping m at 1 puts it between them.
        assert!(s.set_group("p", "m", Some("g"), Some(1)));
        let rows: Vec<String> = s
            .records("p")
            .into_iter()
            .filter(|r| r.state != RecordState::Closed)
            .map(|r| r.id)
            .collect();
        assert_eq!(rows, ["y", "m", "z"]);
        assert_eq!(
            ids(&s),
            ["x", "y", "m", "z"],
            "the closed record keeps its place"
        );
    }

    #[test]
    fn select_is_exclusive_per_project() {
        let mut s = three();
        let mut other = rec("x", RecordState::Live, Some("x"), None);
        other.project = "q".into();
        other.selected = true;
        s.upsert(other);
        assert!(s.select("p", "b"));
        assert!(s.select("p", "c"));
        let sel: Vec<bool> = s.records("p").iter().map(|r| r.selected).collect();
        assert_eq!(sel, [false, false, true]);
        assert!(s.records("q")[0].selected, "another project's choice stays");
        assert!(!s.select("p", "x"), "not this project's");
    }

    #[test]
    fn folds_closed_clearing_and_lookups() {
        let mut s = three();
        s.set_folded("p", "g", true);
        assert_eq!(s.folded("p"), ["g"]);
        s.set_folded("p", "g", false);
        assert!(s.folded("p").is_empty());
        assert_eq!(s.project_of_block("b-b").as_deref(), Some("p"));
        let mut gone = rec("z", RecordState::Closed, None, Some("claude --resume z"));
        gone.position = 3;
        s.upsert(gone.clone());
        let mut also = gone.clone();
        also.id = "y".into();
        s.upsert(also);
        assert!(s.clear_closed("p", Some("z")));
        assert!(!s.clear_closed("p", Some("a")), "only closed records clear");
        assert!(s.clear_closed("p", None));
        assert_eq!(ids(&s), ["a", "b", "c"]);
        assert!(s.remove_project("p"));
        assert!(s.records("p").is_empty());
    }

    #[test]
    fn folds_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let mut s = AgentStore::load(path.clone());
        s.set_folded("p", "g", true);
        assert_eq!(AgentStore::load(path).folded("p"), ["g"]);
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
