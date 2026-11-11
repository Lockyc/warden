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
    /// The records this one was forked from, nearest first; empty for a tab not forked (or
    /// dragged out of its family). The whole chain, so a fork whose source closes or is
    /// forgotten nests under the next one up ([`parent_of`]).
    #[serde(default)]
    pub lineage: Vec<String>,
    /// Its forks are folded away under it.
    #[serde(default)]
    pub folded: bool,
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

/// One harness's resume syntax: the fixed words (program and flags) before the one id, and the
/// words a fork appends after it, `None` where the harness has no verified fork.
struct ResumeShape {
    fixed: &'static [&'static str],
    fork_suffix: Option<&'static [&'static str]>,
}

/// The resume shapes a report may take. A closed table because a forged report must not choose
/// the program or its flags; a new harness, or a new syntax for one, is one row.
const RESUME_SHAPES: &[ResumeShape] = &[
    ResumeShape {
        fixed: &["claude", "--resume"],
        fork_suffix: Some(&["--fork-session"]),
    },
    ResumeShape {
        fixed: &["codex", "resume"],
        fork_suffix: None,
    },
];

/// Why an agent tab can't fork: its harness has no fork form in [`RESUME_SHAPES`].
pub const CANT_FORK: &str = "this agent can't fork";

/// The row of `RESUME_SHAPES` a reported resume command is exactly: the row's fixed words, then
/// one id of `[A-Za-z0-9][A-Za-z0-9-]*` (never a leading `-`, which the CLI would read as a
/// flag), joined by single spaces, nothing else.
fn shape_of(s: &str) -> Option<&'static ResumeShape> {
    let words: Vec<&str> = s.split(' ').collect();
    let (id, fixed) = words.split_last()?;
    let id_ok = id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    RESUME_SHAPES
        .iter()
        .find(|shape| id_ok && shape.fixed == fixed)
}

/// Whether a reported resume command is safe to store: exactly a row of `RESUME_SHAPES`.
fn resume_is_safe(s: &str) -> bool {
    shape_of(s).is_some()
}

/// The command that forks the agent `resume` resumes: a new session branching from the same
/// conversation, the original left as it is. [`CANT_FORK`] for a harness with no fork form.
pub fn fork_command(resume: &str) -> Result<String, &'static str> {
    let suffix = shape_of(resume)
        .and_then(|shape| shape.fork_suffix)
        .ok_or(CANT_FORK)?;
    Ok(std::iter::once(resume)
        .chain(suffix.iter().copied())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Where [`AgentStore::place`] moves a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// At this index among its other siblings not closed, as the list shows them: its source's
    /// other forks, or for a top-level tab its group's other top-level tabs.
    Sibling(usize),
    /// A top-level tab of `group` (trimmed; blank or `None` is ungrouped) at `index` among that
    /// group's other top-level tabs not closed, or after them; a group no row holds yet goes
    /// after the others. A fork placed here leaves its family.
    Root {
        group: Option<String>,
        index: Option<usize>,
    },
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

    /// Move `project`'s record `id`, with its forks, to `to`. Closed records keep their place.
    /// Positions are renumbered in list order ([`in_list_order`]). False for an unknown record.
    pub fn place(&mut self, project: &str, id: &str, to: Place) -> bool {
        let mut list = in_list_order(&self.records(project));
        let Some(at) = list.iter().position(|r| r.id == id) else {
            return false;
        };
        let open = |r: &AgentRecord| r.state != RecordState::Closed;
        match to {
            Place::Sibling(index) => {
                let parent = parent_of(&list, &list[at]);
                let group = list[at].group.clone();
                let mut order: Vec<usize> = (0..list.len())
                    .filter(|&i| {
                        i != at
                            && open(&list[i])
                            && parent_of(&list, &list[i]) == parent
                            && (parent.is_some() || list[i].group == group)
                    })
                    .collect();
                order.insert(index.min(order.len()), at);
                // The siblings trade their slots; each one's forks follow it.
                let mut slots: Vec<u32> = order.iter().map(|&i| list[i].position).collect();
                slots.sort_unstable();
                for (&i, p) in order.iter().zip(slots) {
                    list[i].position = p;
                }
                list = in_list_order(&list);
            }
            Place::Root { group, index } => {
                let group: Option<String> = group
                    .as_deref()
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(Into::into);
                // It leaves its family; its own forks' chains now end at it, and share its group.
                list[at].lineage.clear();
                list[at].group = group.clone();
                for r in &mut list {
                    if let Some(k) = r.lineage.iter().position(|x| x == id) {
                        r.lineage.truncate(k + 1);
                        r.group = group.clone();
                    }
                }
                let (family, mut rest): (Vec<AgentRecord>, Vec<AgentRecord>) = list
                    .into_iter()
                    .partition(|r| r.id == id || r.lineage.iter().any(|x| x == id));
                let members: Vec<usize> = (0..rest.len())
                    .filter(|&i| {
                        rest[i].group == group
                            && open(&rest[i])
                            && parent_of(&rest, &rest[i]).is_none()
                    })
                    .collect();
                // Just past member `i`'s family.
                let end = |i: usize| {
                    let root = &rest[i].id;
                    (i + 1..rest.len())
                        .find(|&j| !rest[j].lineage.contains(root))
                        .unwrap_or(rest.len())
                };
                let to = match (members.get(index.unwrap_or(usize::MAX)), members.last()) {
                    (Some(&i), _) => i,
                    (None, Some(&last)) => end(last),
                    // The first ungrouped record tops the list; a new group follows every other.
                    (None, None) if group.is_none() => 0,
                    (None, None) => rest.len(),
                };
                rest.splice(to..to, family);
                list = rest;
            }
        }
        for (i, r) in list.iter_mut().enumerate() {
            r.position = i as u32;
        }
        self.set_project(project, list);
        true
    }

    /// Fold or unfold the forks under `project`'s record `id`. False for an unknown record.
    pub fn set_family_folded(&mut self, project: &str, id: &str, folded: bool) -> bool {
        let Some(r) = self
            .records
            .iter_mut()
            .find(|r| r.id == id && r.project == project)
        else {
            return false;
        };
        if r.folded != folded {
            r.folded = folded;
            self.save();
        }
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
            self.drop_orphan_folds();
            self.save();
        }
        cleared
    }

    /// `project`'s sessions ended (☠): every resumable record goes to the closed fold, stamped
    /// `now` unless it was already there, and the rest drop. Returns whether a record changed.
    pub fn close_project(&mut self, project: &str, now: u64) -> bool {
        let before = self.records.clone();
        self.records
            .retain(|r| r.project != project || r.resume.is_some());
        for r in self.records.iter_mut().filter(|r| r.project == project) {
            if r.state != RecordState::Closed {
                r.state = RecordState::Closed;
                r.closed_at = Some(now);
            }
            r.window_id = None;
            r.block_id = None;
            r.selected = false;
        }
        let changed = self.records != before;
        if changed {
            self.drop_orphan_folds();
            self.save();
        }
        changed
    }

    /// Forget the folds of projects that no longer hold a record.
    fn drop_orphan_folds(&mut self) {
        let records = &self.records;
        self.folded
            .retain(|f| records.iter().any(|r| r.project == f.project));
    }

    /// Whether `project`, its session gone, has an agent its row can wake — the ghost on its
    /// Rex tab's dot. A `Live` record counts: after a Rex crash or reboot the store reads `Live`
    /// until an open reconciles it asleep.
    pub fn has_wakeable(&self, project: &str) -> bool {
        self.records
            .iter()
            .any(|r| r.project == project && r.state != RecordState::Closed && r.resume.is_some())
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

    /// Insert `record`, or replace the one with its id, and persist.
    #[cfg(test)]
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
            let saved = Saved {
                records: self.records.clone(),
                folded: self.folded.clone(),
            };
            if let Err(e) = crate::session::write_atomic(path, &saved) {
                log::error!("couldn't save agent tabs to {path:?}: {e}");
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
        project, name, group, position, resume, state, window_id, block_id, closed_at, selected,
        lineage, folded
    );
}

/// The id of `r`'s parent among `records` (one project's): the nearest of its lineage still
/// listed and not closed. `None` for a top-level tab.
pub fn parent_of(records: &[AgentRecord], r: &AgentRecord) -> Option<String> {
    r.lineage
        .iter()
        .find(|id| {
            records
                .iter()
                .any(|o| o.id == **id && o.state != RecordState::Closed)
        })
        .cloned()
}

/// How many parents up `r` sits from a top-level tab.
pub fn depth_of(records: &[AgentRecord], r: &AgentRecord) -> u32 {
    let mut depth = 0;
    let mut at = parent_of(records, r);
    // Bounded: only a tampered file could make a cycle.
    while let Some(id) = at.filter(|_| (depth as usize) < records.len()) {
        depth += 1;
        at = records
            .iter()
            .find(|o| o.id == id)
            .and_then(|p| parent_of(records, p));
    }
    depth
}

/// `records` (one project's) in list order: top-level tabs ungrouped above the groups, groups
/// ordered by their first record's position, each by position; every tab's forks directly
/// under it ([`parent_of`]), depth first, each family by position.
pub fn in_list_order(records: &[AgentRecord]) -> Vec<AgentRecord> {
    let parents: Vec<Option<String>> = records.iter().map(|r| parent_of(records, r)).collect();
    let first = |g: &str| {
        records
            .iter()
            .filter(|r| r.group.as_deref() == Some(g))
            .map(|r| r.position)
            .min()
    };
    let key = |&i: &usize| {
        let r = &records[i];
        let group = r.group.clone().map(|g| (first(&g), g));
        (group, r.position)
    };
    let mut roots: Vec<usize> = (0..records.len())
        .filter(|&i| parents[i].is_none())
        .collect();
    roots.sort_by_cached_key(key);
    let mut out = Vec::with_capacity(records.len());
    let mut seen = vec![false; records.len()];
    // Records a tampered lineage cycle leaves unreachable follow the rest, still listed.
    let starts: Vec<usize> = roots.into_iter().chain(0..records.len()).collect();
    for start in starts {
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            out.push(records[i].clone());
            let mut kids: Vec<usize> = (0..records.len())
                .filter(|&j| parents[j].as_deref() == Some(records[i].id.as_str()))
                .collect();
            kids.sort_by_key(|&j| std::cmp::Reverse(records[j].position));
            stack.extend(kids);
        }
    }
    out
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
                lineage: Vec::new(),
                folded: false,
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
            lineage: Vec::new(),
            folded: false,
        }
    }

    #[test]
    fn has_wakeable_needs_a_resumable_record_not_closed() {
        let mut s = AgentStore::default();
        s.upsert(rec(
            "closed",
            RecordState::Closed,
            None,
            Some("claude --resume b"),
        ));
        assert!(
            !s.has_wakeable("p"),
            "a closed agent wakes only from the fold"
        );
        s.upsert(rec("bare", RecordState::Asleep, None, None));
        assert!(!s.has_wakeable("p"), "nothing to resume");
        s.upsert(rec(
            "slept",
            RecordState::Asleep,
            None,
            Some("claude --resume c"),
        ));
        assert!(s.has_wakeable("p"));
        assert!(!s.has_wakeable("other"));
    }

    #[test]
    fn a_live_resumable_record_is_wakeable_once_its_session_is_gone() {
        // After a Rex crash or reboot the store still reads Live until an open reconciles it; the
        // dot asks only when the session is dead, and the next open wakes such a record.
        let mut s = AgentStore::default();
        s.upsert(rec(
            "live",
            RecordState::Live,
            Some("w"),
            Some("claude --resume a"),
        ));
        assert!(s.has_wakeable("p"));
    }

    fn win(id: &str, label: &str) -> AgentWindow {
        AgentWindow {
            window_id: id.into(),
            block_id: Some(format!("b-{id}")),
            label: label.into(),
            layout: None,
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
    fn fork_command_appends_fork_session_for_claude() {
        assert_eq!(
            fork_command("claude --resume abc"),
            Ok("claude --resume abc --fork-session".to_string())
        );
    }

    #[test]
    fn fork_command_refuses_a_harness_without_a_fork_form() {
        assert_eq!(fork_command("codex resume abc"), Err(CANT_FORK));
        assert_eq!(
            fork_command("sh -c id"),
            Err(CANT_FORK),
            "not a resume shape"
        );
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

    /// Place `id` as a top-level tab of `group` at `index`.
    fn put(s: &mut AgentStore, id: &str, group: Option<&str>, index: Option<usize>) -> bool {
        s.place(
            "p",
            id,
            Place::Root {
                group: group.map(Into::into),
                index,
            },
        )
    }

    fn family() -> AgentStore {
        let mut s = AgentStore::default();
        for r in [
            fam("a", 0, &[]),
            fam("a1", 1, &["a"]),
            fam("a1x", 2, &["a1", "a"]),
            fam("a2", 3, &["a"]),
            fam("b", 4, &[]),
        ] {
            s.upsert(r);
        }
        s
    }

    fn by(s: &AgentStore, id: &str) -> AgentRecord {
        s.records("p").into_iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn moving_a_root_carries_its_family() {
        let mut s = family();
        assert!(put(&mut s, "a", None, None));
        assert_eq!(ids(&s), ["b", "a", "a1", "a1x", "a2"]);
        let positions: Vec<u32> = s.records("p").iter().map(|r| r.position).collect();
        assert_eq!(positions, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn sibling_reorder_stays_in_family() {
        let mut s = family();
        assert!(s.place("p", "a2", Place::Sibling(0)));
        assert_eq!(ids(&s), ["a", "a2", "a1", "a1x", "b"]);
        assert_eq!(by(&s, "a2").lineage, ["a"]);
        assert!(s.place("p", "b", Place::Sibling(0)), "roots reorder too");
        assert_eq!(ids(&s), ["b", "a", "a2", "a1", "a1x"]);
    }

    #[test]
    fn rooting_a_fork_detaches_it_with_its_forks() {
        let mut s = family();
        assert!(put(&mut s, "a1", Some("g"), None));
        assert_eq!(ids(&s), ["a", "a2", "b", "a1", "a1x"]);
        assert!(by(&s, "a1").lineage.is_empty());
        assert_eq!(by(&s, "a1x").lineage, ["a1"]);
        assert_eq!(by(&s, "a1x").group.as_deref(), Some("g"));
        // Its own source closing no longer reaches back to a.
        let mut a1 = by(&s, "a1");
        a1.state = RecordState::Closed;
        s.upsert(a1);
        let recs = s.records("p");
        assert_eq!(parent_of(&recs, &by(&s, "a1x")), None);
    }

    #[test]
    fn regrouping_a_root_writes_through_to_its_forks() {
        let mut s = family();
        assert!(put(&mut s, "a", Some("g"), None));
        for id in ["a", "a1", "a1x", "a2"] {
            assert_eq!(by(&s, id).group.as_deref(), Some("g"), "{id}");
        }
        assert_eq!(ids(&s), ["b", "a", "a1", "a1x", "a2"]);
    }

    #[test]
    fn rooting_lands_before_or_after_whole_families() {
        let mut s = family();
        assert!(put(&mut s, "b", None, Some(0)));
        assert_eq!(ids(&s), ["b", "a", "a1", "a1x", "a2"], "before a's family");
        assert!(put(&mut s, "b", None, None));
        assert_eq!(ids(&s), ["a", "a1", "a1x", "a2", "b"], "after it");
    }

    #[test]
    fn a_family_folds() {
        let mut s = family();
        assert!(s.set_family_folded("p", "a", true));
        assert!(by(&s, "a").folded);
        assert!(!s.set_family_folded("p", "nope", true));
    }

    #[test]
    fn grouping_keeps_ungrouped_above_groups_and_renumbers() {
        let mut s = three();
        assert!(put(&mut s, "a", Some("work"), None));
        assert_eq!(ids(&s), ["b", "c", "a"], "a new group goes last");
        assert!(put(&mut s, "c", Some(" work "), Some(0)));
        assert_eq!(ids(&s), ["b", "c", "a"], "first in its group");
        assert_eq!(s.records("p")[1].group.as_deref(), Some("work"));
        assert!(put(&mut s, "a", None, Some(0)));
        assert_eq!(ids(&s), ["a", "b", "c"], "ungrouped, at the top");
        assert!(put(&mut s, "b", Some(""), None), "blank clears");
        assert_eq!(ids(&s), ["a", "b", "c"]);
        let positions: Vec<u32> = s.records("p").iter().map(|r| r.position).collect();
        assert_eq!(positions, [0, 1, 2]);
        assert!(!put(&mut s, "nope", None, None));
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
        assert!(put(&mut s, "m", Some("g"), Some(1)));
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
    }

    #[test]
    fn ending_a_project_closes_its_resumable_agents_and_drops_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AgentStore::load(dir.path().join("agents.json"));
        let mut live = rec(
            "live",
            RecordState::Live,
            Some("w1"),
            Some("claude --resume l"),
        );
        live.selected = true;
        s.upsert(live);
        s.upsert(rec(
            "asleep",
            RecordState::Asleep,
            None,
            Some("claude --resume a"),
        ));
        let mut was = rec("was", RecordState::Closed, None, Some("claude --resume w"));
        was.closed_at = Some(5);
        s.upsert(was);
        s.upsert(rec("bare", RecordState::Live, Some("w2"), None));
        let mut other = rec("other", RecordState::Live, Some("w3"), Some("claude"));
        other.project = "q".into();
        s.upsert(other);
        s.set_folded("p", "g", true);

        assert!(s.close_project("p", 9));
        let by = |id: &str| s.records("p").into_iter().find(|r| r.id == id);
        for id in ["live", "asleep"] {
            let r = by(id).unwrap();
            assert_eq!(r.state, RecordState::Closed, "{id}");
            assert_eq!(r.closed_at, Some(9), "{id}");
            assert_eq!((r.window_id, r.block_id, r.selected), (None, None, false));
        }
        assert_eq!(
            by("was").unwrap().closed_at,
            Some(5),
            "already closed keeps its stamp"
        );
        assert_eq!(by("bare"), None, "nothing to reopen it with: dropped");
        assert_eq!(
            s.records("q")[0].state,
            RecordState::Live,
            "other projects untouched"
        );
        assert!(!s.has_wakeable("p"), "nothing left to ghost the dot");
        assert_eq!(s.folded("p"), ["g"], "groups keep their fold for a reopen");
        assert!(!s.close_project("p", 10), "a second end changes nothing");

        s.clear_closed("p", None);
        assert!(
            s.folded("p").is_empty(),
            "the last record takes its project's folds"
        );
    }

    #[test]
    fn folds_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let mut s = AgentStore::load(path.clone());
        s.set_folded("p", "g", true);
        assert_eq!(AgentStore::load(path).folded("p"), ["g"]);
    }

    /// A live, resumable record at `position`, forked down `lineage` (nearest first).
    fn fam(id: &str, position: u32, lineage: &[&str]) -> AgentRecord {
        let mut r = rec(id, RecordState::Live, Some(id), Some("claude --resume x"));
        r.position = position;
        r.lineage = lineage.iter().map(ToString::to_string).collect();
        r
    }

    fn order(records: &[AgentRecord]) -> Vec<(String, u32)> {
        in_list_order(records)
            .iter()
            .map(|r| (r.id.clone(), depth_of(records, r)))
            .collect()
    }

    fn named(v: &[(&str, u32)]) -> Vec<(String, u32)> {
        v.iter().map(|(id, d)| (id.to_string(), *d)).collect()
    }

    #[test]
    fn forks_list_depth_first_under_their_source() {
        let recs = [
            fam("a", 0, &[]),
            fam("b", 1, &[]),
            fam("a1x", 6, &["a1", "a"]),
            fam("a1", 5, &["a"]),
        ];
        assert_eq!(
            order(&recs),
            named(&[("a", 0), ("a1", 1), ("a1x", 2), ("b", 0)])
        );
    }

    #[test]
    fn closed_source_promotes_its_forks_and_reopening_renests() {
        let mut recs = [
            fam("a", 0, &[]),
            fam("a1", 1, &[]),
            fam("a1x", 2, &["a1", "a"]),
        ];
        recs[1].lineage = vec!["a".into()];
        recs[1].state = RecordState::Closed;
        assert_eq!(parent_of(&recs, &recs[2]).as_deref(), Some("a"));
        recs[1].state = RecordState::Live;
        assert_eq!(parent_of(&recs, &recs[2]).as_deref(), Some("a1"));
    }

    #[test]
    fn cleared_source_promotes_to_next_ancestor() {
        let recs = [fam("a", 0, &[]), fam("a1x", 2, &["a1", "a"])];
        assert_eq!(parent_of(&recs, &recs[1]).as_deref(), Some("a"));
        assert_eq!(order(&recs), named(&[("a", 0), ("a1x", 1)]));
    }

    #[test]
    fn old_file_without_lineage_loads_top_level() {
        let json = r#"{"id":"a","project":"p","name":"a","group":null,"position":0,
            "resume":null,"state":"Live","window_id":null,"block_id":null,
            "closed_at":null,"selected":false}"#;
        let r: AgentRecord = serde_json::from_str(json).unwrap();
        assert!(r.lineage.is_empty());
        assert!(!r.folded);
    }

    #[test]
    fn reconcile_dropping_a_mid_family_record_keeps_order_contiguous() {
        let mut a1 = fam("a1", 1, &["a"]);
        a1.resume = None;
        let recs = [
            fam("a", 0, &[]),
            a1,
            fam("a1x", 2, &["a1", "a"]),
            fam("b", 3, &[]),
        ];
        let out = reconcile(
            "p",
            &recs,
            &[win("a", "a"), win("a1x", "a1x"), win("b", "b")],
            true,
            1,
        );
        assert_eq!(order(&out), named(&[("a", 0), ("a1x", 1), ("b", 0)]));
    }

    #[test]
    fn a_tampered_lineage_cycle_still_lists_every_record() {
        let recs = [fam("a", 0, &["b"]), fam("b", 1, &["a"]), fam("c", 2, &[])];
        let mut ids: Vec<String> = in_list_order(&recs).into_iter().map(|r| r.id).collect();
        ids.sort();
        assert_eq!(ids, ["a", "b", "c"]);
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
