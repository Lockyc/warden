use crate::colour::Colour;
use crate::model::{Config, Profile, Tab};

#[derive(Debug, Clone, PartialEq)]
pub struct Reconciliation {
    pub open: Vec<Profile>,
    pub close: Vec<String>,
    pub update: Vec<ProfileUpdate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProfileUpdate {
    pub name: String,
    pub colour: Option<Colour>,
    pub add_tabs: Vec<Tab>,
    pub remove_tabs: Vec<String>,
    pub retitle_window: bool,
}

fn find<'a>(profiles: &'a [Profile], name: &str) -> Option<&'a Profile> {
    profiles.iter().find(|p| p.name == name)
}

pub fn reconcile(old: &Config, new: &Config) -> Reconciliation {
    let mut open = Vec::new();
    let mut close = Vec::new();
    let mut update = Vec::new();

    // closed: in old, not in new
    for op in &old.profiles {
        if find(&new.profiles, &op.name).is_none() {
            close.push(op.name.clone());
        }
    }

    for np in &new.profiles {
        match find(&old.profiles, &np.name) {
            None => open.push(np.clone()),
            Some(op) => {
                let colour = (op.colour != np.colour).then_some(np.colour);
                let old_keys: Vec<&str> = op.tabs.iter().map(|t| t.key.as_str()).collect();
                let new_keys: Vec<&str> = np.tabs.iter().map(|t| t.key.as_str()).collect();
                let add_tabs: Vec<Tab> = np
                    .tabs
                    .iter()
                    .filter(|t| !old_keys.contains(&t.key.as_str()))
                    .cloned()
                    .collect();
                let remove_tabs: Vec<String> = op
                    .tabs
                    .iter()
                    .filter(|t| !new_keys.contains(&t.key.as_str()))
                    .map(|t| t.key.clone())
                    .collect();
                if colour.is_some() || !add_tabs.is_empty() || !remove_tabs.is_empty() {
                    update.push(ProfileUpdate {
                        name: np.name.clone(),
                        colour,
                        add_tabs,
                        remove_tabs,
                        retitle_window: false,
                    });
                }
            }
        }
    }

    Reconciliation { open, close, update }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::parse;
    use crate::resolve::resolve;

    fn cfg(s: &str) -> Config {
        resolve(parse(s).unwrap()).unwrap().0
    }

    const BASE: &str = r##"
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  title = "locus"
  dir = "/tmp/locus"
"##;

    #[test]
    fn added_profile_goes_to_open() {
        let old = cfg("");
        let new = cfg(BASE);
        let r = reconcile(&old, &new);
        assert_eq!(r.open.len(), 1);
        assert_eq!(r.open[0].name, "work");
        assert!(r.close.is_empty() && r.update.is_empty());
    }

    #[test]
    fn removed_profile_goes_to_close() {
        let r = reconcile(&cfg(BASE), &cfg(""));
        assert_eq!(r.close, vec!["work".to_string()]);
        assert!(r.open.is_empty() && r.update.is_empty());
    }

    #[test]
    fn identical_config_is_noop() {
        let r = reconcile(&cfg(BASE), &cfg(BASE));
        assert!(r.open.is_empty() && r.close.is_empty() && r.update.is_empty());
    }

    #[test]
    fn colour_change_emits_update_with_colour() {
        let new = cfg(&BASE.replace("#0f8a8a", "#112233"));
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.update.len(), 1);
        assert_eq!(r.update[0].colour, Some(Colour { r: 0x11, g: 0x22, b: 0x33 }));
        assert!(r.update[0].add_tabs.is_empty() && r.update[0].remove_tabs.is_empty());
    }

    #[test]
    fn added_and_removed_tabs_within_kept_profile() {
        let new = cfg(r##"
[[profile]]
name = "work"
colour = "#0f8a8a"
  [[profile.tab]]
  title = "ops"
  dir = "/tmp/ops"
"##);
        let r = reconcile(&cfg(BASE), &new);
        assert_eq!(r.update.len(), 1);
        let u = &r.update[0];
        assert_eq!(u.add_tabs.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), vec!["ops"]);
        assert_eq!(u.remove_tabs, vec!["locus".to_string()]);
        assert_eq!(u.colour, None);
    }
}
