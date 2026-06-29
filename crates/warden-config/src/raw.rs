use serde::Deserialize;

// `shell` and `cmd` cascade global → window → tab (nearest set level wins; see resolve.rs).
// Both are optional at every level — a missing field inherits, an empty `cmd = ""` opts out.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawConfig {
    pub shell: Option<String>,
    pub cmd: Option<String>,
    pub probe: Option<String>,
    pub kill: Option<String>,
    /// Seconds between background session-probe passes (global only). `None` →
    /// default 5; `Some(0)` → focus/refresh-only (no timer). See resolve.rs.
    pub probe_interval: Option<u64>,
    // When true, warden rewrites this config file formatted on each clean hot-reload
    // (via config-core's format_file). Optional; a missing field resolves to false.
    pub format_on_save: Option<bool>,
    // ⌘1/⌘2 menu behaviour: "jump" (default, ⌘1–9 jump to position) or "cycle"
    // (⌘1 = next tab, ⌘2 = prev; jumps shift to ⌘3–9). Validated in resolve.rs.
    pub tab_digit_keys: Option<String>,
    #[serde(default, rename = "window")]
    pub windows: Vec<RawWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawWindow {
    pub title: String,
    pub colour: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub shell: Option<String>,
    pub cmd: Option<String>,
    pub probe: Option<String>,
    pub kill: Option<String>,
    // Loose tabs declared directly under the window (`[[window.tab]]`) — ungrouped,
    // rendered in a headerless section before any named groups.
    #[serde(default, rename = "tab")]
    pub tabs: Vec<RawTab>,
    // Named groups (`[[window.group]]`), each holding its own `[[window.group.tab]]`s.
    // Resolution flattens loose tabs + each group's tabs into one ordered `Tab` list,
    // tagging every tab with its group (loose = `None`); see resolve.rs.
    #[serde(default, rename = "group")]
    pub groups: Vec<RawGroup>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawGroup {
    pub name: String,
    #[serde(default, rename = "tab")]
    pub tabs: Vec<RawTab>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawTab {
    pub title: Option<String>,
    pub dir: String,
    pub shell: Option<String>,
    pub cmd: Option<String>,
    pub probe: Option<String>,
    pub kill: Option<String>,
    #[serde(default)]
    pub load_on_open: bool,
}

pub fn parse(toml_str: &str) -> Result<RawConfig, toml::de::Error> {
    toml::from_str(toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
shell = "fish -l"
cmd = "amux"

[[window]]
title = "work"
colour = "#0f8a8a"
shell = "zsh"
cmd = "tmux"

  [[window.tab]]
  dir = "~/Developer/alpha"
  cmd = "tmux"
  load_on_open = true

  [[window.tab]]
  title = "ops"
  dir = "~/Developer/api"
"##;

    #[test]
    fn parses_full_sample() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.shell.as_deref(), Some("fish -l"));
        assert_eq!(cfg.cmd.as_deref(), Some("amux"));
        assert_eq!(cfg.windows.len(), 1);
        let p = &cfg.windows[0];
        assert_eq!(p.title, "work");
        assert_eq!(p.colour.as_deref(), Some("#0f8a8a"));
        assert_eq!(p.shell.as_deref(), Some("zsh"));
        assert_eq!(p.cmd.as_deref(), Some("tmux"));
        assert_eq!(p.tabs.len(), 2);
        assert_eq!(p.tabs[0].cmd.as_deref(), Some("tmux"));
        assert!(p.tabs[0].load_on_open);
        assert_eq!(p.tabs[1].title.as_deref(), Some("ops"));
        assert!(p.tabs[1].cmd.is_none()); // inherits via cascade
        assert!(!p.tabs[1].load_on_open); // serde default
    }

    #[test]
    fn empty_config_has_no_windows() {
        let cfg = parse("").unwrap();
        assert!(cfg.windows.is_empty());
        assert!(cfg.shell.is_none());
        assert!(cfg.cmd.is_none());
    }

    #[test]
    fn parses_loose_tabs_and_groups_in_order() {
        let cfg = parse(
            r##"
[[window]]
title = "work"
colour = "#0f8a8a"

  [[window.tab]]
  dir = "~/notes"

  [[window.group]]
  name = "frontend"
    [[window.group.tab]]
    dir = "~/dev/web"
    [[window.group.tab]]
    dir = "~/dev/design"

  [[window.group]]
  name = "backend"
    [[window.group.tab]]
    dir = "~/dev/api"
"##,
        )
        .unwrap();
        let w = &cfg.windows[0];
        // Loose tabs and group blocks are independent TOML arrays.
        assert_eq!(w.tabs.len(), 1);
        assert_eq!(w.tabs[0].dir, "~/notes");
        assert_eq!(w.groups.len(), 2);
        assert_eq!(w.groups[0].name, "frontend");
        assert_eq!(w.groups[0].tabs.len(), 2);
        assert_eq!(w.groups[1].name, "backend");
        assert_eq!(w.groups[1].tabs[0].dir, "~/dev/api");
    }
}
