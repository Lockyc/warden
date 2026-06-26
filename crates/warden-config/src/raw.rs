use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawConfig {
    pub default_cmd: Option<String>,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<RawProfile>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawProfile {
    pub name: String,
    pub colour: String,
    pub icon: Option<String>,
    #[serde(default, rename = "tab")]
    pub tabs: Vec<RawTab>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawTab {
    pub title: Option<String>,
    pub dir: String,
    pub cmd: Option<String>,
    #[serde(default)]
    pub keep_alive: bool,
}

pub fn parse(toml_str: &str) -> Result<RawConfig, toml::de::Error> {
    toml::from_str(toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
default_cmd = "fish -l"

[[profile]]
name = "work"
colour = "#0f8a8a"

  [[profile.tab]]
  dir = "~/Developer/locus"
  cmd = "tmux"
  keep_alive = true

  [[profile.tab]]
  title = "ops"
  dir = "~/Developer/api"
"##;

    #[test]
    fn parses_full_sample() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.default_cmd.as_deref(), Some("fish -l"));
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "work");
        assert_eq!(p.colour, "#0f8a8a");
        assert_eq!(p.tabs.len(), 2);
        assert_eq!(p.tabs[0].cmd.as_deref(), Some("tmux"));
        assert!(p.tabs[0].keep_alive);
        assert_eq!(p.tabs[1].title.as_deref(), Some("ops"));
        assert!(!p.tabs[1].keep_alive); // serde default
    }

    #[test]
    fn empty_config_has_no_profiles() {
        let cfg = parse("").unwrap();
        assert!(cfg.profiles.is_empty());
        assert!(cfg.default_cmd.is_none());
    }
}
