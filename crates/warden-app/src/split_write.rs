//! Saving a tab's split to the config: the right-click menu and ⌘D write through here, onto the
//! tab's own config entry (a curated tab, or a `[[window.root.tab]]` override for a discovered
//! project), and the config watcher's hot-reload applies it like any other edit.

use std::path::{Path, PathBuf};

use warden_config::edit::{self, At, RootAt};
use warden_config::raw::{RawConfig, RawRoot, RawSplit, RawTab, RawWindow};
use warden_config::resolve::cascade_split_over;
use warden_config::toml_edit::Value;
use warden_config::{normalize_dir_key, Tab};

/// What to change about a tab's saved split.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SplitEdit {
    /// Turn the split on (`split = true`); nothing to write when the tab already resolves one.
    Enable,
    /// Save the second pane's share; dropped from the file when it equals what the tab inherits.
    Size(f64),
    /// Drop the tab's own `size`, back to the inherited one.
    ResetSize,
}

/// Where a tab's `split` lives in the file.
enum Origin<'a> {
    Curated {
        group: Option<&'a str>,
        by: (&'static str, String),
        raw: &'a RawTab,
    },
    Discovered {
        root: &'a RawRoot,
        /// The project's `dir` as the file spells it (an existing override's, else a new one).
        dir: String,
        existing: Option<&'a RawSplit>,
    },
}

/// A raw `dir` as `Tab.dir` resolves it (tilde-expanded, anchored on the config's directory).
fn resolved_dir(raw: &str, base: &Path) -> PathBuf {
    let d = warden_config::expand_tilde(raw.trim());
    if d.is_relative() {
        base.join(d)
    } else {
        d
    }
}

fn dir_key(raw: &str, base: &Path) -> String {
    normalize_dir_key(&resolved_dir(raw, base))
}

/// The dir string for a new override, in the style the root's own `dir` is written.
fn new_dir_string(root_dir: &str, project: &Path) -> String {
    let home = warden_config::expand_tilde("~");
    if root_dir.trim().starts_with('~') {
        if let Ok(rest) = project.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    project.display().to_string()
}

fn locate<'a>(window: &'a RawWindow, tab: &Tab, base: &Path) -> Result<Origin<'a>, String> {
    let matches = |t: &RawTab| match &tab.id {
        Some(id) => t.id.as_deref().map(str::trim) == Some(id.as_str()),
        None => {
            t.id.as_deref().is_none_or(|i| i.trim().is_empty()) && dir_key(&t.dir, base) == tab.key
        }
    };
    let by = |t: &RawTab| match &tab.id {
        Some(id) => ("id", id.clone()),
        None => ("dir", t.dir.trim().to_string()),
    };
    if let Some(t) = window.extra.tabs.iter().find(|t| matches(t)) {
        return Ok(Origin::Curated {
            group: None,
            by: by(t),
            raw: t,
        });
    }
    for g in &window.groups {
        if let Some(t) = g.tabs.iter().find(|t| matches(t)) {
            return Ok(Origin::Curated {
                group: Some(g.name.trim()),
                by: by(t),
                raw: t,
            });
        }
    }
    let root = window
        .extra
        .roots
        .iter()
        .find(|r| {
            let named = warden_config::resolve_root_dir(r.name.as_deref(), &r.dir, r.depth)
                .map(|d| d.name)
                .ok();
            named.is_some() && named.as_deref() == tab.group.as_deref()
        })
        .ok_or_else(|| format!("no config entry for tab {:?}", tab.title))?;
    let existing_tab = root.tabs.iter().find(|t| dir_key(&t.dir, base) == tab.key);
    Ok(Origin::Discovered {
        root,
        dir: existing_tab
            .map(|t| t.dir.trim().to_string())
            .unwrap_or_else(|| new_dir_string(&root.dir, &tab.dir)),
        existing: existing_tab.and_then(|t| t.split.as_ref()),
    })
}

/// `split` with its `size` dropped, as the cascade sees a level that leaves the size unset.
fn without_size(s: Option<&RawSplit>) -> Option<RawSplit> {
    s.map(|s| match s {
        RawSplit::On(t) => {
            let mut t = t.clone();
            t.size = None;
            RawSplit::On(t)
        }
        RawSplit::Off(b) => RawSplit::Off(*b),
    })
}

/// Write `edit` onto `tab`'s config entry in window `window`. `raw` is the parsed `config`.
/// Refuses (leaving the file untouched) while the file fails to parse or resolve.
pub fn write_split(
    config: &Path,
    raw: &RawConfig,
    window: &str,
    tab: &Tab,
    edit: SplitEdit,
) -> Result<(), String> {
    warden_config::load(config).map_err(|e| format!("config not saved: {e}"))?;
    if edit == SplitEdit::Enable && tab.split.is_some() {
        return Ok(());
    }
    let w = raw_window(raw, window)?;
    let origin = locate(w, tab, config.parent().unwrap_or(Path::new("")))?;
    let (own, far) = levels(&origin);
    let cascade = cascade_with(raw, w, far);

    let (keys, new): (&[&str], Option<Value>) = match edit {
        SplitEdit::Enable => (&["split"], Some(Value::from(true))),
        SplitEdit::Size(s) => {
            let s = stored_size(s);
            if (s - inherited(&cascade, own)?).abs() < 0.005 {
                remove_size(&cascade, own)
            } else {
                (&["split", "size"], Some(Value::from(s)))
            }
        }
        SplitEdit::ResetSize => remove_size(&cascade, own),
    };
    apply(config, window, &origin, keys, new)
}

/// A size as the file keeps it: two decimals.
pub fn stored_size(s: f64) -> f64 {
    (s * 100.0).round() / 100.0
}

fn raw_window<'a>(raw: &'a RawConfig, window: &str) -> Result<&'a RawWindow, String> {
    raw.windows
        .iter()
        .find(|w| w.title.trim() == window.trim())
        .ok_or_else(|| format!("window {window:?} not in the config"))
}

/// The tab's own split, and the levels above it nearest first (a discovered project's root),
/// for the cascade the tab inherits through.
fn levels<'a>(origin: &Origin<'a>) -> (Option<&'a RawSplit>, Vec<Option<&'a RawSplit>>) {
    match origin {
        Origin::Curated { raw, .. } => (raw.split.as_ref(), vec![]),
        Origin::Discovered { root, existing, .. } => (*existing, vec![root.split.as_ref()]),
    }
}

/// The split a tab resolves to with `first` as its own level, under the levels `far` and the
/// window's and global ones.
fn cascade_with<'a>(
    raw: &'a RawConfig,
    w: &'a RawWindow,
    far: Vec<Option<&'a RawSplit>>,
) -> impl Fn(Option<&RawSplit>) -> Result<Option<warden_config::Split>, String> + 'a {
    move |first| {
        let mut near = vec![first];
        near.extend(far.iter().copied());
        cascade_split_over(raw, w, &near).map_err(|e| e.to_string())
    }
}

/// The size the tab gets without a `size` of its own.
fn inherited(cascade: CascadeFn, own: Option<&RawSplit>) -> Result<f64, String> {
    Ok(cascade(without_size(own).as_ref())?
        .map(|sp| sp.size)
        .unwrap_or(warden_config::resolve::DEFAULT_SPLIT_SIZE))
}

/// The config warden runs from (`config_path`), parsed as it stands now.
fn current() -> Result<(PathBuf, RawConfig), String> {
    let config = warden_config::config_path();
    let not_read = |e: &dyn std::fmt::Display| format!("config not saved: {e}");
    let text = std::fs::read_to_string(&config).map_err(|e| not_read(&e))?;
    let raw = warden_config::raw::parse(&text).map_err(|e| not_read(&e))?;
    Ok((config, raw))
}

/// [`write_split`] onto the config warden runs from.
pub fn save_split(window: &str, tab: &Tab, edit: SplitEdit) -> Result<(), String> {
    let (config, raw) = current()?;
    write_split(&config, &raw, window, tab, edit)
}

/// The size `tab` in window `window` inherits without a `size` of its own — what
/// [`SplitEdit::ResetSize`] goes back to — read from the config at `config`.
pub fn inherited_size(
    config: &Path,
    raw: &RawConfig,
    window: &str,
    tab: &Tab,
) -> Result<f64, String> {
    let w = raw_window(raw, window)?;
    let origin = locate(w, tab, config.parent().unwrap_or(Path::new("")))?;
    let (own, far) = levels(&origin);
    inherited(&cascade_with(raw, w, far), own)
}

/// [`inherited_size`] in the config warden runs from.
pub fn current_inherited_size(window: &str, tab: &Tab) -> Result<f64, String> {
    let (config, raw) = current()?;
    inherited_size(&config, &raw, window, tab)
}

type CascadeFn<'a> = &'a dyn Fn(Option<&RawSplit>) -> Result<Option<warden_config::Split>, String>;

/// Remove `split.size` — unless that would empty the tab's own split table and leave nothing
/// above it to keep the split on, in which case the table collapses to `split = true`.
fn remove_size<'k>(cascade: CascadeFn, own: Option<&RawSplit>) -> (&'k [&'k str], Option<Value>) {
    let only_size = match own {
        Some(RawSplit::On(t)) => t.side.is_none() && t.cmd.is_none() && t.panes.is_none(),
        _ => false,
    };
    let above = cascade(None).ok().flatten().is_some();
    if only_size && !above {
        (&["split"], Some(Value::from(true)))
    } else {
        (&["split", "size"], None)
    }
}

fn apply(
    config: &Path,
    window: &str,
    origin: &Origin,
    keys: &[&str],
    new: Option<Value>,
) -> Result<(), String> {
    match origin {
        Origin::Curated { group, by, .. } => edit::set_path(
            config,
            At {
                window,
                group: *group,
                tab: Some((by.0, by.1.as_str())),
            },
            keys,
            new,
        ),
        Origin::Discovered { root, dir, .. } => edit::set_root_tab_path(
            config,
            RootAt {
                window,
                root_dir: root.dir.trim(),
                dir,
            },
            keys,
            new,
        ),
    }
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup(toml: &str) -> (TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, toml).unwrap();
        (d, p)
    }

    fn run(
        p: &Path,
        window: &str,
        pick: impl Fn(&warden_config::Config) -> Tab,
        e: SplitEdit,
    ) -> Result<String, String> {
        let text = std::fs::read_to_string(p).unwrap();
        let raw = warden_config::raw::parse(&text).unwrap();
        let loaded = warden_config::load(p).unwrap();
        let tab = pick(&loaded.config);
        write_split(p, &raw, window, &tab, e)?;
        Ok(std::fs::read_to_string(p).unwrap())
    }

    fn tab_titled(c: &warden_config::Config, t: &str) -> Tab {
        c.windows[0]
            .tabs
            .iter()
            .find(|x| x.title == t)
            .unwrap()
            .clone()
    }

    const GROUPED: &str = r##"
[[window]]
title = "W"

[[window.group]]
name = "G"

[[window.group.tab]]
dir = "/tmp/a"
title = "A"
"##;

    #[test]
    fn enable_on_curated_group_tab_writes_split_true() {
        let (_d, p) = setup(GROUPED);
        let out = run(&p, "W", |c| tab_titled(c, "A"), SplitEdit::Enable).unwrap();
        assert!(out.contains("split = true"), "{out}");
        let c = warden_config::load(&p).unwrap().config;
        assert!(tab_titled(&c, "A").split.is_some());
    }

    #[test]
    fn enable_when_inherited_is_noop() {
        let (_d, p) = setup(&format!("split = true\n{GROUPED}"));
        let before = std::fs::read_to_string(&p).unwrap();
        let out = run(&p, "W", |c| tab_titled(c, "A"), SplitEdit::Enable).unwrap();
        assert_eq!(out, before);
    }

    #[test]
    fn size_writes_tab_key_keeps_parent_side() {
        let toml = r##"
[[window]]
title = "W"

[window.split]
side = "left"

[[window.tab]]
dir = "/tmp/a"
title = "A"
"##;
        let (_d, p) = setup(toml);
        run(&p, "W", |c| tab_titled(c, "A"), SplitEdit::Size(0.333)).unwrap();
        let c = warden_config::load(&p).unwrap().config;
        let s = tab_titled(&c, "A").split.unwrap();
        assert_eq!(s.side, warden_config::SplitSide::Left);
        assert_eq!(s.size, 0.33);
    }

    #[test]
    fn size_equal_to_inherited_removes_key() {
        let toml = r##"
[[window]]
title = "W"

[window.split]
size = 0.3

[[window.tab]]
dir = "/tmp/a"
title = "A"
split = { size = 0.4, side = "left" }
"##;
        let (_d, p) = setup(toml);
        let out = run(&p, "W", |c| tab_titled(c, "A"), SplitEdit::Size(0.3)).unwrap();
        assert!(!out.contains("0.4"), "{out}");
        let c = warden_config::load(&p).unwrap().config;
        let s = tab_titled(&c, "A").split.unwrap();
        assert_eq!((s.size, s.side), (0.3, warden_config::SplitSide::Left));
    }

    #[test]
    fn discovered_project_writes_root_tab() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("proj");
        std::fs::create_dir_all(root.join("p1")).unwrap();
        let toml = format!(
            "[[window]]\ntitle = \"W\"\n\n[[window.root]]\nname = \"R\"\ndir = \"{}\"\nsplit = {{ side = \"left\" }}\n",
            root.display()
        );
        let p = d.path().join("config.toml");
        std::fs::write(&p, toml).unwrap();
        let raw = warden_config::raw::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let tab = Tab {
            dir: root.join("p1"),
            key: normalize_dir_key(&root.join("p1")),
            group: Some("R".into()),
            ..bare_tab()
        };
        write_split(&p, &raw, "W", &tab, SplitEdit::Size(0.7)).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("[[window.root.tab]]"), "{out}");
        let c = warden_config::load(&p).unwrap().config;
        let o = &c.windows[0].roots[0].overrides[0];
        let s = o.split.as_ref().unwrap();
        assert_eq!((s.size, s.side), (0.7, warden_config::SplitSide::Left));
    }

    #[test]
    fn unnamed_root_discovered_project_saves() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("proj");
        std::fs::create_dir_all(root.join("p1")).unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            format!(
                "[[window]]\ntitle = \"W\"\n\n[[window.root]]\ndir = \"{}\"\n",
                root.display()
            ),
        )
        .unwrap();
        let raw = warden_config::raw::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let tab = Tab {
            dir: root.join("p1"),
            key: normalize_dir_key(&root.join("p1")),
            group: Some("proj".into()),
            ..bare_tab()
        };
        write_split(&p, &raw, "W", &tab, SplitEdit::Enable).unwrap();
        let c = warden_config::load(&p).unwrap().config;
        assert!(c.windows[0].roots[0].overrides[0].split.is_some());
    }

    #[test]
    fn tilde_spelled_dir_is_found() {
        let toml = "[[window]]\ntitle = \"W\"\n\n[[window.tab]]\ndir = \"~/warden-split-write-test\"\ntitle = \"X\"\n";
        let (_d, p) = setup(toml);
        let out = run(&p, "W", |c| tab_titled(c, "X"), SplitEdit::Size(0.3)).unwrap();
        assert!(out.contains("dir = \"~/warden-split-write-test\""), "{out}");
        let c = warden_config::load(&p).unwrap().config;
        assert_eq!(tab_titled(&c, "X").split.unwrap().size, 0.3);
    }

    #[test]
    fn id_addresses_the_right_tab_when_dirs_are_shared() {
        let toml = "[[window]]\ntitle = \"W\"\n\n[[window.tab]]\nid = \"one\"\ndir = \"/tmp/a\"\ntitle = \"One\"\n\n[[window.tab]]\nid = \"two\"\ndir = \"/tmp/a\"\ntitle = \"Two\"\n";
        let (_d, p) = setup(toml);
        run(&p, "W", |c| tab_titled(c, "Two"), SplitEdit::Enable).unwrap();
        let c = warden_config::load(&p).unwrap().config;
        assert!(tab_titled(&c, "Two").split.is_some());
        assert!(tab_titled(&c, "One").split.is_none());
    }

    #[test]
    fn inherited_size_ignores_the_tabs_own_size() {
        let toml = "[[window]]\ntitle = \"W\"\n\n[window.split]\nsize = 0.3\n\n[[window.tab]]\ndir = \"/tmp/a\"\ntitle = \"A\"\nsplit = { size = 0.4 }\n\n[[window.tab]]\ndir = \"/tmp/b\"\ntitle = \"B\"\nsplit = true\n";
        let (_d, p) = setup(toml);
        let raw = warden_config::raw::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let c = warden_config::load(&p).unwrap().config;
        let a = inherited_size(&p, &raw, "W", &tab_titled(&c, "A")).unwrap();
        let b = inherited_size(&p, &raw, "W", &tab_titled(&c, "B")).unwrap();
        assert_eq!((a, b), (0.3, 0.3));
    }

    #[test]
    fn reset_size_drops_only_the_size() {
        let toml = "[[window]]\ntitle = \"W\"\n\n[[window.tab]]\ndir = \"/tmp/a\"\ntitle = \"A\"\nsplit = { size = 0.4, side = \"left\" }\n";
        let (_d, p) = setup(toml);
        run(&p, "W", |c| tab_titled(c, "A"), SplitEdit::ResetSize).unwrap();
        let c = warden_config::load(&p).unwrap().config;
        let s = tab_titled(&c, "A").split.unwrap();
        assert_eq!((s.size, s.side), (0.5, warden_config::SplitSide::Left));
    }

    #[test]
    fn dropping_the_only_split_key_keeps_the_split_on() {
        let toml = "[[window]]\ntitle = \"W\"\n\n[[window.tab]]\ndir = \"/tmp/a\"\ntitle = \"A\"\nsplit = { size = 0.4 }\n";
        let (_d, p) = setup(toml);
        let out = run(&p, "W", |c| tab_titled(c, "A"), SplitEdit::ResetSize).unwrap();
        assert!(out.contains("split = true"), "{out}");
        let c = warden_config::load(&p).unwrap().config;
        assert_eq!(tab_titled(&c, "A").split.unwrap().size, 0.5);
    }

    fn bare_tab() -> Tab {
        Tab {
            id: None,
            key: String::new(),
            title: "p1".into(),
            dir: PathBuf::new(),
            shell: "sh".into(),
            startup: None,
            load_on_open: false,
            group: None,
            probe: None,
            kill: None,
            suspend: None,
            resume: None,
            server: None,
            split: None,
            colour: None,
        }
    }

    #[test]
    fn split_write_refuses_unparseable_config() {
        let (_d, p) = setup(GROUPED);
        let text = std::fs::read_to_string(&p).unwrap();
        let raw = warden_config::raw::parse(&text).unwrap();
        let tab = tab_titled(&warden_config::load(&p).unwrap().config, "A");
        let broken = format!("{text}\n[[[ nope\n");
        std::fs::write(&p, &broken).unwrap();
        assert!(write_split(&p, &raw, "W", &tab, SplitEdit::Enable).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken);
    }
}
