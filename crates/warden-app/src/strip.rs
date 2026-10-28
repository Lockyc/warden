//! The bottom strip: the project's name on its colour, under a Rex tab's agent pane — a glance
//! that says which project the input box in front of you belongs to. Plain tabs have none.
//!
//! The colour is `plan::project_colours`, the one assignment the sidebar's row mark reads too.

use crate::plan::project_colours;
use warden_config::Config;

/// The summary rows under the name. Task 9 maps the selected agent's summary onto it.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct StripRows {
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

/// Tab `tab_id`'s strip, or `None` for a tab that is not a Rex tab (or not in `config`).
pub fn strip_dto(config: &Config, tab_id: &str) -> Option<StripDto> {
    let tab = config
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .find(|t| t.key == tab_id && t.server.is_some())?;
    let colour = project_colours(config).remove(&tab.key)?;
    Some(StripDto {
        tab_id: tab.key.clone(),
        name: tab.title.clone(),
        colour,
        rows: None,
    })
}

/// Send tab `tab_id`'s strip (`warden:strip`) to every window, like `warden:agents`: it is keyed
/// by tab, the same in any window holding the tab, which ignores one it doesn't show.
#[cfg(target_os = "macos")]
pub fn emit(app: &tauri::AppHandle, config: &Config, tab_id: &str) {
    use tauri::Emitter;
    if let Some(dto) = strip_dto(config, tab_id) {
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

    #[test]
    fn strip_dto_names_and_colours_rex_tab() {
        let cfg = config();
        let tab = &cfg.windows[0].tabs[1];
        let dto = strip_dto(&cfg, &tab.key).expect("a Rex tab has a strip");
        assert_eq!(dto.tab_id, tab.key);
        assert_eq!(dto.name, "Rex Project");
        assert_eq!(dto.colour, "#112233");
        assert_eq!(dto.rows, None);
    }

    #[test]
    fn no_strip_for_plain_tab() {
        let cfg = config();
        let tab = &cfg.windows[0].tabs[0];
        assert_eq!(strip_dto(&cfg, &tab.key), None);
        assert_eq!(strip_dto(&cfg, "nope"), None);
    }
}
