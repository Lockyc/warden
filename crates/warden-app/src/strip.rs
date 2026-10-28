//! The bottom strip: the project's name on its colour, under a Rex tab's agent pane — a glance
//! that says which project the input box in front of you belongs to — with the selected agent
//! tab's subject beside the name and its done/now/next rows below ([`crate::rex::summary`]).
//! Plain tabs have none.
//!
//! The colour is `plan::project_colours`, the one assignment the sidebar's row mark reads too.

use crate::plan::project_colours;
use crate::rex::agents::{AgentStore, RecordState};
use warden_config::Config;
use warden_summary::StandRows;

/// The selected agent tab's summary.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct StripRows {
    /// What the conversation is about, shown after the project name.
    pub subject: Option<String>,
    pub done: Option<String>,
    pub now: Option<String>,
    pub next: Option<String>,
}

/// What `warden:strip` and `strip_init` carry for project tab `tab_id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StripDto {
    pub tab_id: String,
    pub name: String,
    /// `#rrggbb`.
    pub colour: String,
    pub rows: Option<StripRows>,
}

/// Tab `tab_id`'s strip, or `None` for a tab that is not a Rex tab (or not in `config`). The rows
/// are its selected agent tab's summary, asleep or live, from `agents`.
pub fn strip_dto(config: &Config, agents: &AgentStore, tab_id: &str) -> Option<StripDto> {
    let tab = config
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .find(|t| t.key == tab_id && t.server.is_some())?;
    let colour = project_colours(config).remove(&tab.key)?;
    let rows = agents
        .records(&tab.key)
        .into_iter()
        .find(|r| r.selected && r.state != RecordState::Closed)
        .and_then(|r| rows_of(r.summary, r.subject));
    Some(StripDto {
        tab_id: tab.key.clone(),
        name: tab.title.clone(),
        colour,
        rows,
    })
}

/// A record's summary as the strip shows it. The subject is the stand line's own, else the
/// record's stable label (a stand line can lead with none). `None` with neither.
fn rows_of(summary: Option<StandRows>, label: Option<String>) -> Option<StripRows> {
    let stand = summary.unwrap_or_default();
    let rows = StripRows {
        subject: stand.subject.or(label),
        done: stand.done,
        now: stand.now,
        next: stand.next,
    };
    (rows != StripRows::default()).then_some(rows)
}

/// Send tab `tab_id`'s strip (`warden:strip`) to every window, like `warden:agents`: it is keyed
/// by tab, the same in any window holding the tab, which ignores one it doesn't show. Locks the
/// agent store, so a caller may hold `ManagerState` but not the store.
#[cfg(target_os = "macos")]
pub fn emit(app: &tauri::AppHandle, config: &Config, tab_id: &str) {
    use tauri::{Emitter, Manager};
    let Some(store) = app.try_state::<std::sync::Mutex<AgentStore>>() else {
        return;
    };
    let dto = {
        let agents = store.lock().unwrap_or_else(|e| e.into_inner());
        strip_dto(config, &agents, tab_id)
    };
    if let Some(dto) = dto {
        let _ = app.emit("warden:strip", dto);
    }
}

/// Re-send every Rex tab's strip: after a config change, which may have renamed or recoloured one.
#[cfg(target_os = "macos")]
pub fn emit_all(app: &tauri::AppHandle, config: &Config) {
    let keys: Vec<String> = config
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .filter(|t| t.server.is_some())
        .map(|t| t.key.clone())
        .collect();
    for key in keys {
        emit(app, config, &key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::agent_open::tests::record;

    fn config() -> Config {
        warden_config::resolve::resolve(
            warden_config::raw::parse(
                r##"
[[window]]
title = "w"
  [[window.tab]]
  title = "plain"
  dir = "/a"
  [[window.tab]]
  title = "Rex Project"
  dir = "/b"
  server = "local"
  project_colour = "#112233"
"##,
            )
            .unwrap(),
        )
        .unwrap()
        .0
    }

    fn summarised(id: &str, project: &str, state: RecordState, selected: bool) -> AgentStore {
        let mut r = record(id, state, None, Some("claude --resume x"));
        r.project = project.into();
        r.selected = selected;
        r.subject = Some("label subject".into());
        r.summary = Some(StandRows {
            subject: Some("refactoring the strip".into()),
            done: Some("split the css".into()),
            now: Some("wiring rows".into()),
            next: None,
        });
        let mut s = AgentStore::default();
        s.upsert(r);
        s
    }

    #[test]
    fn strip_dto_names_and_colours_rex_tab() {
        let cfg = config();
        let tab = &cfg.windows[0].tabs[1];
        let dto = strip_dto(&cfg, &AgentStore::default(), &tab.key).expect("a Rex tab has one");
        assert_eq!(dto.tab_id, tab.key);
        assert_eq!(dto.name, "Rex Project");
        assert_eq!(dto.colour, "#112233");
        assert_eq!(dto.rows, None, "no agent tab: no rows");
    }

    #[test]
    fn no_strip_for_plain_tab() {
        let cfg = config();
        let tab = &cfg.windows[0].tabs[0];
        let none = AgentStore::default();
        assert_eq!(strip_dto(&cfg, &none, &tab.key), None);
        assert_eq!(strip_dto(&cfg, &none, "nope"), None);
    }

    #[test]
    fn strip_dto_uses_selected_records_rows() {
        let cfg = config();
        let key = cfg.windows[0].tabs[1].key.clone();
        let mut s = summarised("a", &key, RecordState::Live, true);
        let mut other = record("b", RecordState::Live, None, None);
        other.project = key.clone();
        other.summary = Some(StandRows {
            now: Some("not this one".into()),
            ..Default::default()
        });
        s.upsert(other);
        let rows = strip_dto(&cfg, &s, &key).unwrap().rows;
        assert_eq!(
            rows,
            Some(StripRows {
                subject: Some("refactoring the strip".into()),
                done: Some("split the css".into()),
                now: Some("wiring rows".into()),
                next: None,
            })
        );

        let unselected = summarised("a", &key, RecordState::Live, false);
        assert_eq!(strip_dto(&cfg, &unselected, &key).unwrap().rows, None);
    }

    #[test]
    fn strip_dto_for_asleep_selected_record_keeps_rows() {
        let cfg = config();
        let key = cfg.windows[0].tabs[1].key.clone();
        let s = summarised("a", &key, RecordState::Asleep, true);
        let rows = strip_dto(&cfg, &s, &key)
            .unwrap()
            .rows
            .expect("asleep keeps rows");
        assert_eq!(rows.now.as_deref(), Some("wiring rows"));
    }

    #[test]
    fn strip_subject_falls_back_to_the_label() {
        let cfg = config();
        let key = cfg.windows[0].tabs[1].key.clone();
        let mut s = summarised("a", &key, RecordState::Live, true);
        let mut r = s.record("a").unwrap();
        r.summary = None;
        s.upsert(r);
        let rows = strip_dto(&cfg, &s, &key)
            .unwrap()
            .rows
            .expect("a subject alone shows");
        assert_eq!(rows.subject.as_deref(), Some("label subject"));
        assert_eq!((rows.done, rows.now, rows.next), (None, None, None));
    }
}
