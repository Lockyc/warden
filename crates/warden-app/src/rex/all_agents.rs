//! The All Agents list: every Rex project's awake agent tabs in one list, grouped by project.

use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};

use super::agents::{self, AgentStore};
use super::attention::Attention;
use super::lifecycle::{self, AgentListDto, AgentRowDto, AllAgentsGroupDto};
use super::spawn::unix_now;
use super::windows::BlockStatus;
use super::{lock, RexHub};
use crate::registry::{agent_key, ALL_AGENTS_ID};
use crate::ManagerState;

/// A project tab the All Agents list draws from, with the title and colour its group shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRef {
    pub id: String,
    pub title: String,
    pub colour: String,
}

/// The awake agent tabs of every project in `projects`, in that order, each project's in its own
/// list's order: a row's `id` and `parent` are pane keys ([`agent_key`]), a fork nests under its
/// nearest awake ancestor, and a row is `amber` when it needs you. It has no Asleep or Cold Cache
/// group, no named groups and no closed fold, and selects nothing: the page owns its selection.
pub fn all_agents_list(
    projects: &[ProjectRef],
    store: &AgentStore,
    attention: &Attention,
    status: impl Fn(&str) -> Option<BlockStatus>,
) -> AgentListDto {
    let now = unix_now();
    let mut rows = Vec::new();
    let mut groups = Vec::new();
    for p in projects {
        let (records, all, _) = lifecycle::agent_rows(&p.id, store, attention, &status);
        let awake = |id: &str| all.iter().any(|r| r.id == id && r.state != "asleep");
        let mut kept: Vec<AgentRowDto> = Vec::new();
        for r in all.iter().filter(|r| r.state != "asleep") {
            let up = records
                .iter()
                .find(|rec| rec.id == r.id)
                .and_then(|rec| agents::nearest_ancestor(rec, awake))
                .map(|id| agent_key(&p.id, &id));
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
                record: Some(r.id.clone()),
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
        lifecycle::nest(&mut kept, now);
        // No row is asleep, so none is in the Asleep group; a cold one stays in its project.
        for (r, need) in kept.iter_mut().zip(needs) {
            r.amber |= need;
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
        all_agents: true,
    }
}

/// The pane key of every row in `list`: in the All Agents list, each awake agent tab's.
pub fn awake_keys(list: &AgentListDto) -> Vec<String> {
    list.rows.iter().map(|r| r.id.clone()).collect()
}

/// Why an All Agents row can't show: its agent has no terminal bound in Rex.
const NOT_AWAKE: &str = "that agent tab isn't awake";

/// The All Agents list as it stands, over every open window's projects
/// ([`WindowManager::rex_projects`](crate::manager::WindowManager::rex_projects)): what
/// `warden:agents` carries for [`ALL_AGENTS_ID`], and what the chrome pulls (`all_agents_init`).
/// Locks `ManagerState`, so callers must hold none.
pub fn current_list(app: &AppHandle) -> Option<AgentListDto> {
    let projects = app.state::<ManagerState>().lock().rex_projects();
    let store = app.try_state::<Mutex<AgentStore>>()?;
    let hub = app.try_state::<RexHub>()?;
    let attention = app.try_state::<Mutex<Attention>>()?;
    let dto = all_agents_list(&projects, &lock(&store), &lock(&attention), |b| {
        hub.block_status(b)
    });
    Some(dto)
}

/// Send the All Agents list (`warden:agents`) to every window, when one shows the row, and have
/// each All Agents tab drop the agents no longer awake
/// ([`WindowManager::retain_all_agents`](crate::manager::WindowManager::retain_all_agents)): a
/// window whose shown agent went selects the neighbour that answers, else blanks its pane. The
/// surfaces change on the main thread. Locks `ManagerState`, so callers must hold none.
pub fn emit_all_agents(app: &AppHandle) {
    if !app.state::<ManagerState>().lock().has_all_agents() {
        return;
    }
    let Some(list) = current_list(app) else {
        return;
    };
    let _ = app.emit("warden:agents", &list);
    let awake = awake_keys(&list);
    let main = app.clone();
    let _ = app.run_on_main_thread(move || {
        let moved = main
            .state::<ManagerState>()
            .lock()
            .retain_all_agents(&awake);
        for (label, next) in moved {
            match next.map(|key| select(&main, &label, &key)) {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    log::warn!("{label}: All Agents couldn't lean to a neighbour: {e}");
                    refresh(&main, &label);
                }
                None => refresh(&main, &label),
            }
        }
    });
}

/// Show agent `key` ([`agent_key`]) in window `label`'s All Agents tab, on the block the hub
/// bound it to ([`RexHub::agent_target`]), and repaint that window (main thread). `Err` when the
/// agent has no bound block, the window has no All Agents tab, or the surface failed to spawn.
pub fn select(app: &AppHandle, label: &str, key: &str) -> Result<(), String> {
    let target = app
        .try_state::<RexHub>()
        .and_then(|hub| hub.agent_target(key))
        .ok_or(NOT_AWAKE)?;
    let shown = {
        let st = app.state::<ManagerState>();
        let mut m = st.lock();
        let ws = m.windows.get_mut(label).ok_or("window not found")?;
        let shown = ws.registry.select_all_agent(key, target);
        m.record_session(app, label);
        shown
    };
    refresh(app, label);
    shown
}

/// Push window `label`'s snapshot (`warden:refresh`, label-stamped).
fn refresh(app: &AppHandle, label: &str) {
    let dto = app.state::<ManagerState>().lock().init_dto(label);
    if let Some(dto) = dto {
        let _ = app.emit_to(label, "warden:refresh", dto);
    }
}

#[cfg(test)]
mod tests {
    use super::{all_agents_list, awake_keys, ProjectRef};
    use crate::registry::{agent_key, ALL_AGENTS_ID};
    use crate::rex::agent_open::tests::{record, store};
    use crate::rex::agents::{AgentRecord, RecordState};
    use crate::rex::attention::Attention;
    use crate::rex::lifecycle::agent_list;
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
        assert!(list.all_agents);
        let rows: Vec<(&str, Option<&str>, Option<&str>)> = list
            .rows
            .iter()
            .map(|r| (r.id.as_str(), r.project.as_deref(), r.record.as_deref()))
            .collect();
        let (a, d) = (agent_key("p1", "a"), agent_key("p2", "d"));
        assert_eq!(
            rows,
            [
                (a.as_str(), Some("p1"), Some("a")),
                (d.as_str(), Some("p2"), Some("d"))
            ]
        );
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
        let own = agent_list("p", &s.lock().unwrap(), &Attention::default(), cold);
        assert!(
            own.rows.iter().all(|r| r.cold_group),
            "the project's own list puts them in Cold Cache"
        );
        assert!(!own.all_agents);
        assert!(own
            .rows
            .iter()
            .all(|r| r.project.is_none() && r.record.is_none()));
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
    fn a_project_whose_only_needy_row_is_folded_away_is_amber() {
        let mut parent = in_project("p", record("a", RecordState::Live, Some("window:1"), None));
        parent.folded = true;
        let mut fork = in_project("p", record("a1", RecordState::Live, Some("window:2"), None));
        fork.lineage = vec!["a".into()];
        let s = store(vec![parent, fork]);
        let st = |block: &str| match block {
            "block:2" => Some(status(AgentState::Blocked, None)),
            _ => Some(status(AgentState::Idle, None)),
        };
        let list = all_agents_list(
            &[project("p")],
            &s.lock().unwrap(),
            &Attention::default(),
            st,
        );
        let rows: Vec<(String, bool, bool)> = list
            .rows
            .iter()
            .map(|r| (r.id.clone(), r.hidden, r.amber))
            .collect();
        assert_eq!(
            rows,
            [
                (agent_key("p", "a"), false, true),
                (agent_key("p", "a1"), true, true)
            ],
            "the fork is folded away; its parent shows its need"
        );
        assert!(list.projects[0].amber);
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
