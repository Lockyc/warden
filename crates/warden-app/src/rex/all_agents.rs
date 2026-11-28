//! The All Agents list: every Rex project's awake agent tabs in one list, grouped by project.

use super::agents::AgentStore;
use super::attention::Attention;
use super::lifecycle::{self, AgentListDto, AgentRowDto};
use super::windows::BlockStatus;
use crate::registry::{agent_key, ALL_AGENTS_ID};

/// A project tab the All Agents list draws from, with the title and colour its group shows.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the All Agents tab is wired next")
)]
pub struct ProjectRef {
    pub id: String,
    pub title: String,
    pub colour: String,
}

/// A project's group in the All Agents list.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllAgentsGroupDto {
    pub project: String,
    pub title: String,
    pub colour: String,
    /// A row in it needs you.
    pub amber: bool,
}

/// The awake agent tabs of every project in `projects`, in that order, each project's in its own
/// list's order: a row's `id` and `parent` are pane keys ([`agent_key`]), a fork nests under its
/// nearest awake ancestor, and a row is `amber` when it needs you. It has no Asleep or Cold Cache
/// group, no named groups and no closed fold, and selects nothing: the page owns its selection.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the All Agents tab is wired next")
)]
pub fn all_agents_list(
    projects: &[ProjectRef],
    store: &AgentStore,
    attention: &Attention,
    status: impl Fn(&str) -> Option<BlockStatus>,
) -> AgentListDto {
    let mut rows = Vec::new();
    let mut groups = Vec::new();
    for p in projects {
        let (_, all, _) = lifecycle::agent_rows(&p.id, store, attention, &status);
        let awake = |id: &str| all.iter().any(|r| r.id == id && r.state != "asleep");
        let parent = |id: &str| all.iter().find(|r| r.id == id)?.parent.clone();
        let mut kept: Vec<AgentRowDto> = Vec::new();
        for r in all.iter().filter(|r| r.state != "asleep") {
            let mut up = r.parent.clone();
            // Bounded: only a tampered file could make a cycle.
            for _ in 0..all.len() {
                match up {
                    Some(ref id) if !awake(id) => up = parent(id),
                    _ => break,
                }
            }
            let up = up.map(|id| agent_key(&p.id, &id));
            let depth = up
                .as_ref()
                .and_then(|k| kept.iter().find(|o| o.id == *k))
                .map_or(0, |o| o.depth + 1);
            kept.push(AgentRowDto {
                id: agent_key(&p.id, &r.id),
                parent: up,
                depth,
                selected: false,
                project: Some(p.id.clone()),
                ..r.clone()
            });
        }
        if kept.is_empty() {
            continue;
        }
        let needs: Vec<bool> = kept.iter().map(|r| r.amber).collect();
        groups.push(AllAgentsGroupDto {
            project: p.id.clone(),
            title: p.title.clone(),
            colour: p.colour.clone(),
            amber: needs.contains(&true),
        });
        lifecycle::nest(&mut kept, 0);
        for (r, need) in kept.iter_mut().zip(needs) {
            r.amber |= need;
            r.asleep_group = false;
            r.cold_group = false;
        }
        rows.extend(kept);
    }
    AgentListDto {
        tab_id: ALL_AGENTS_ID.to_owned(),
        rows,
        groups: Vec::new(),
        closed: Vec::new(),
        projects: groups,
    }
}

/// The pane key of every row in `list`: in the All Agents list, each awake agent tab's.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the All Agents tab is wired next")
)]
pub fn awake_keys(list: &AgentListDto) -> Vec<String> {
    list.rows.iter().map(|r| r.id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::{all_agents_list, awake_keys, ProjectRef};
    use crate::registry::{agent_key, ALL_AGENTS_ID};
    use crate::rex::agent_open::tests::{record, store};
    use crate::rex::agents::{AgentRecord, RecordState};
    use crate::rex::attention::Attention;
    use crate::rex::windows::{AgentState, BlockStatus};

    fn in_project(project: &str, mut r: AgentRecord) -> AgentRecord {
        r.project = project.into();
        r
    }

    fn project(id: &str) -> ProjectRef {
        ProjectRef {
            id: id.into(),
            title: format!("{id} title"),
            colour: format!("#{id}"),
        }
    }

    fn status(state: AgentState, at: Option<&str>) -> BlockStatus {
        BlockStatus {
            state,
            kind: None,
            msg: None,
            title: None,
            asked: None,
            answered: None,
            said: None,
            topic: None,
            story: None,
            needs: None,
            background: None,
            cache: None,
            at: at.map(Into::into),
        }
    }

    fn idle(_: &str) -> Option<BlockStatus> {
        Some(status(AgentState::Idle, None))
    }

    #[test]
    fn lists_only_awake_rows_grouped_by_project_in_project_order() {
        let s = store(vec![
            in_project("p1", record("a", RecordState::Live, Some("window:1"), None)),
            in_project(
                "p1",
                record("b", RecordState::Asleep, None, Some("claude --resume b")),
            ),
            in_project(
                "p1",
                record("c", RecordState::Closed, None, Some("claude --resume c")),
            ),
            in_project("p2", record("d", RecordState::Live, Some("window:4"), None)),
        ]);
        let list = all_agents_list(
            &[project("p1"), project("p2")],
            &s.lock().unwrap(),
            &Attention::default(),
            idle,
        );
        assert_eq!(list.tab_id, ALL_AGENTS_ID);
        let rows: Vec<(&str, Option<&str>)> = list
            .rows
            .iter()
            .map(|r| (r.id.as_str(), r.project.as_deref()))
            .collect();
        let (a, d) = (agent_key("p1", "a"), agent_key("p2", "d"));
        assert_eq!(rows, [(a.as_str(), Some("p1")), (d.as_str(), Some("p2"))]);
        assert_eq!(awake_keys(&list), [a, d]);
        let groups: Vec<(&str, &str, &str, bool)> = list
            .projects
            .iter()
            .map(|g| {
                (
                    g.project.as_str(),
                    g.title.as_str(),
                    g.colour.as_str(),
                    g.amber,
                )
            })
            .collect();
        assert_eq!(
            groups,
            [
                ("p1", "p1 title", "#p1", false),
                ("p2", "p2 title", "#p2", false)
            ]
        );
        assert!(list.groups.is_empty());
        assert!(list.closed.is_empty());
    }

    #[test]
    fn keeps_fork_nesting_within_a_project() {
        let parent = in_project("p", record("a", RecordState::Live, Some("window:1"), None));
        let mut fork = in_project("p", record("a1", RecordState::Live, Some("window:2"), None));
        fork.lineage = vec!["a".into()];
        let s = store(vec![parent, fork]);
        let list = all_agents_list(
            &[project("p")],
            &s.lock().unwrap(),
            &Attention::default(),
            idle,
        );
        let rows: Vec<(String, Option<&str>, u32, u32)> = list
            .rows
            .iter()
            .map(|r| (r.id.clone(), r.parent.as_deref(), r.depth, r.forks))
            .collect();
        let a = agent_key("p", "a");
        assert_eq!(
            rows,
            [
                (a.clone(), None, 0, 1),
                (agent_key("p", "a1"), Some(a.as_str()), 1, 0)
            ]
        );
    }

    #[test]
    fn a_live_fork_under_an_asleep_parent_surfaces_at_depth_zero() {
        let parent = in_project(
            "p",
            record("a", RecordState::Asleep, None, Some("claude --resume a")),
        );
        let mut fork = in_project("p", record("a1", RecordState::Live, Some("window:2"), None));
        fork.lineage = vec!["a".into()];
        let s = store(vec![parent, fork]);
        let list = all_agents_list(
            &[project("p")],
            &s.lock().unwrap(),
            &Attention::default(),
            idle,
        );
        let rows: Vec<(String, Option<&str>, u32)> = list
            .rows
            .iter()
            .map(|r| (r.id.clone(), r.parent.as_deref(), r.depth))
            .collect();
        assert_eq!(rows, [(agent_key("p", "a1"), None, 0)]);
    }

    #[test]
    fn a_fork_under_an_asleep_middle_nests_under_the_nearest_awake_ancestor() {
        let top = in_project("p", record("a", RecordState::Live, Some("window:1"), None));
        let mut middle = in_project(
            "p",
            record("a1", RecordState::Asleep, None, Some("claude --resume a1")),
        );
        middle.lineage = vec!["a".into()];
        let mut leaf = in_project(
            "p",
            record("a1x", RecordState::Live, Some("window:3"), None),
        );
        leaf.lineage = vec!["a1".into(), "a".into()];
        let s = store(vec![top, middle, leaf]);
        let list = all_agents_list(
            &[project("p")],
            &s.lock().unwrap(),
            &Attention::default(),
            idle,
        );
        let rows: Vec<(String, Option<&str>, u32)> = list
            .rows
            .iter()
            .map(|r| (r.id.clone(), r.parent.as_deref(), r.depth))
            .collect();
        let a = agent_key("p", "a");
        assert_eq!(
            rows,
            [
                (a.clone(), None, 0),
                (agent_key("p", "a1x"), Some(a.as_str()), 1)
            ]
        );
    }

    #[test]
    fn amber_comes_from_needs_you() {
        let s = store(vec![
            in_project("p1", record("a", RecordState::Live, Some("window:1"), None)),
            in_project("p1", record("b", RecordState::Live, Some("window:2"), None)),
            in_project("p2", record("c", RecordState::Live, Some("window:3"), None)),
            in_project("p3", record("d", RecordState::Live, Some("window:4"), None)),
        ]);
        let st = |block: &str| match block {
            "block:1" => Some(status(AgentState::Blocked, None)),
            "block:3" => Some(status(AgentState::Done, Some("7"))),
            _ => Some(status(AgentState::Idle, None)),
        };
        let list = all_agents_list(
            &[project("p1"), project("p2"), project("p3")],
            &s.lock().unwrap(),
            &Attention::default(),
            st,
        );
        let rows: Vec<(String, bool, Option<&str>)> = list
            .rows
            .iter()
            .map(|r| (r.id.clone(), r.amber, r.unseen.as_deref()))
            .collect();
        assert_eq!(
            rows,
            [
                (agent_key("p1", "a"), true, None),
                (agent_key("p1", "b"), false, None),
                (agent_key("p2", "c"), true, Some("7")),
                (agent_key("p3", "d"), false, None),
            ]
        );
        let groups: Vec<(&str, bool)> = list
            .projects
            .iter()
            .map(|g| (g.project.as_str(), g.amber))
            .collect();
        assert_eq!(groups, [("p1", true), ("p2", true), ("p3", false)]);
    }

    #[test]
    fn cold_rows_stay_in_their_project() {
        let mut selected = in_project("p", record("a", RecordState::Live, Some("window:1"), None));
        selected.selected = true;
        let s = store(vec![
            selected,
            in_project("p", record("b", RecordState::Live, Some("window:2"), None)),
        ]);
        // A cache that went cold at the epoch's first second.
        let cold = |_: &str| {
            Some(BlockStatus {
                cache: Some("1".into()),
                ..status(AgentState::Idle, None)
            })
        };
        let list = all_agents_list(
            &[project("p")],
            &s.lock().unwrap(),
            &Attention::default(),
            cold,
        );
        assert_eq!(list.rows.len(), 2);
        for r in &list.rows {
            assert!(!r.cold_group && !r.asleep_group && !r.selected, "{}", r.id);
        }
    }

    #[test]
    fn a_project_with_no_awake_row_has_no_group() {
        let s = store(vec![
            in_project(
                "p1",
                record("a", RecordState::Asleep, None, Some("claude --resume a")),
            ),
            in_project("p2", record("b", RecordState::Live, Some("window:2"), None)),
        ]);
        let list = all_agents_list(
            &[project("p1"), project("p2")],
            &s.lock().unwrap(),
            &Attention::default(),
            idle,
        );
        let groups: Vec<&str> = list.projects.iter().map(|g| g.project.as_str()).collect();
        assert_eq!(groups, ["p2"]);
    }
}
