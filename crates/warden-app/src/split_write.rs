//! Saving a tab's split to the config: the right-click menu and ⌘D write through here, onto the
//! tab's own config entry (a curated tab, or a `[[window.root.tab]]` override for a discovered
//! project), and the config watcher's hot-reload applies it like any other edit.

use std::path::{Path, PathBuf};

use warden_config::edit::{self, At, RootAt};
use warden_config::raw::{RawConfig, RawRoot, RawSplit, RawTab, RawWindow};
use warden_config::resolve::{cascade_split_over, PANE_SIZE_MAX, PANE_SIZE_MIN};
use warden_config::toml_edit::{Array, InlineTable, Value};
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
    // The file edit addresses an id-less tab by its dir, which finds the owner array's FIRST tab
    // with that dir — so an earlier same-dir tab (one given an `id` to tell them apart) would
    // take the write.
    let curated = |tabs: &'a [RawTab], group: Option<&'a str>| {
        let i = tabs.iter().position(&matches)?;
        let t = &tabs[i];
        let by = match &tab.id {
            Some(id) => ("id", id.clone()),
            None => {
                let dir = t.dir.trim();
                if tabs[..i].iter().any(|e| e.dir.trim() == dir) {
                    return Some(Err("give this tab an `id` to save its split".to_string()));
                }
                ("dir", dir.to_string())
            }
        };
        Some(Ok(Origin::Curated { group, by, raw: t }))
    };
    if let Some(found) = curated(&window.extra.tabs, None) {
        return found;
    }
    for g in &window.groups {
        if let Some(found) = curated(&g.tabs, Some(g.name.trim())) {
            return found;
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
    apply(config, window, &origin, &[(keys, new)])
}

/// Save the panes' shares of the column, top to bottom, onto `tab`'s config entry in window
/// `window`: each declared `pane` gains its `size`, or an inherited stack is copied onto the tab's
/// own entry with the new sizes. Refuses (leaving the file untouched) while the file fails to
/// load, when the tab has no split, or when `sizes` doesn't match the panes it declares.
pub fn write_pane_sizes(
    config: &Path,
    raw: &RawConfig,
    window: &str,
    tab: &Tab,
    sizes: &[f64],
) -> Result<(), String> {
    warden_config::load(config).map_err(|e| format!("config not saved: {e}"))?;
    let split = tab.split.as_ref().ok_or("the tab has no split")?;
    if split.panes.len() != sizes.len() {
        return Err(format!(
            "the column has {} panes; the config declares {}",
            sizes.len(),
            split.panes.len()
        ));
    }
    let w = raw_window(raw, window)?;
    let origin = locate(w, tab, config.parent().unwrap_or(Path::new("")))?;
    let (own, _) = levels(&origin);
    let own_panes = match own {
        Some(RawSplit::On(t)) => t.panes.as_ref().is_some_and(|p| p.len() == sizes.len()),
        _ => false,
    };
    let sizes = pane_sizes(sizes);
    if own_panes {
        let idx: Vec<String> = (0..sizes.len()).map(|i| i.to_string()).collect();
        let keys: Vec<[&str; 4]> = idx.iter().map(|i| ["split", "pane", i, "size"]).collect();
        let edits: Vec<(&[&str], Option<Value>)> = keys
            .iter()
            .zip(&sizes)
            .map(|(k, s)| (&k[..], Some(Value::from(*s))))
            .collect();
        return apply(config, window, &origin, &edits);
    }
    let mut arr = Array::new();
    for (p, s) in split.panes.iter().zip(sizes) {
        let mut t = InlineTable::new();
        if let Some(c) = p.startup.as_deref().filter(|c| !c.is_empty()) {
            t.insert("cmd", Value::from(c));
        }
        t.insert("size", Value::from(s));
        arr.push(Value::InlineTable(t));
    }
    apply(
        config,
        window,
        &origin,
        &[(&["split", "pane"], Some(Value::Array(arr)))],
    )
}

/// [`write_pane_sizes`] onto the config warden runs from.
pub fn save_pane_sizes(window: &str, tab: &Tab, sizes: &[f64]) -> Result<(), String> {
    let (config, raw) = current()?;
    write_pane_sizes(&config, &raw, window, tab, sizes)
}

/// A size as the file keeps it: two decimals.
pub fn stored_size(s: f64) -> f64 {
    (s * 100.0).round() / 100.0
}

/// The column's shares as the file keeps them, each within the band a pane `size` validates to
/// — Rex lets a pane sit narrower (or wider) than a config pane may — with what that adds taken
/// from the largest, so the shares still sum to 1 and none normalises below `PANE_SIZE_MIN`.
fn pane_sizes(sizes: &[f64]) -> Vec<f64> {
    let mut v: Vec<f64> = sizes
        .iter()
        .map(|s| stored_size(*s).clamp(PANE_SIZE_MIN, PANE_SIZE_MAX))
        .collect();
    let excess = stored_size(v.iter().sum::<f64>() - 1.0);
    if excess > 0.0 {
        if let Some(big) = v.iter_mut().max_by(|a, b| a.total_cmp(b)) {
            *big = stored_size((*big - excess).max(PANE_SIZE_MIN));
        }
    }
    v
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
    let not_read = |e: &dyn std::fmt::Display| format!("config not read: {e}");
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

/// Write `edits` (each a key path and its new value, `None` removing it) onto the entry `origin`
/// names, in order, as one file save.
fn apply(
    config: &Path,
    window: &str,
    origin: &Origin,
    edits: &[(&[&str], Option<Value>)],
) -> Result<(), String> {
    match origin {
        Origin::Curated { group, by, .. } => edit::set_paths(
            config,
            At {
                window,
                group: *group,
                tab: Some((by.0, by.1.as_str())),
            },
            edits,
        ),
        Origin::Discovered { root, dir, .. } => edit::set_root_tab_paths(
            config,
            RootAt {
                window,
                root_dir: root.dir.trim(),
                dir,
            },
            edits,
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
    fn idless_tab_sharing_an_earlier_tabs_dir_refuses() {
        let toml = "[[window]]\ntitle = \"W\"\n\n[[window.tab]]\nid = \"one\"\ndir = \"/tmp/a\"\ntitle = \"One\"\n\n[[window.tab]]\ndir = \"/tmp/a\"\ntitle = \"Two\"\n";
        let (_d, p) = setup(toml);
        let before = std::fs::read_to_string(&p).unwrap();
        let err = run(&p, "W", |c| tab_titled(c, "Two"), SplitEdit::Enable).unwrap_err();
        assert_eq!(err, "give this tab an `id` to save its split");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
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

    const STACK3: &str = r##"
[[window]]
title = "W"

[[window.tab]]
dir = "/tmp/a"
title = "A"
server = "s"

[[window.tab.split.pane]]
cmd = "one"
size = 0.5

[[window.tab.split.pane]]
cmd = "two"
size = 0.25

[[window.tab.split.pane]]
cmd = "three"
size = 0.25
"##;

    fn panes(c: &warden_config::Config, t: &str) -> Vec<(f64, Option<String>)> {
        tab_titled(c, t)
            .split
            .unwrap()
            .panes
            .iter()
            .map(|p| (p.size, p.startup.clone()))
            .collect()
    }

    fn run_sizes(p: &Path, t: &str, sizes: &[f64]) -> Result<String, String> {
        let raw = warden_config::raw::parse(&std::fs::read_to_string(p).unwrap()).unwrap();
        let tab = tab_titled(&warden_config::load(p).unwrap().config, t);
        write_pane_sizes(p, &raw, "W", &tab, sizes)?;
        Ok(std::fs::read_to_string(p).unwrap())
    }

    #[test]
    fn pane_sizes_written_into_the_tabs_own_stack() {
        let (_d, p) = setup(STACK3);
        let out = run_sizes(&p, "A", &[0.2, 0.3, 0.5]).unwrap();
        assert!(out.contains("cmd = \"two\""), "{out}");
        let c = warden_config::load(&p).unwrap().config;
        assert_eq!(
            panes(&c, "A"),
            vec![
                (0.2, Some("one".into())),
                (0.3, Some("two".into())),
                (0.5, Some("three".into()))
            ]
        );
    }

    #[test]
    fn pane_sizes_on_a_mixed_stack_land_in_one_save() {
        // The last pane sized, the first two not: the first pane's new size written on its own
        // would overflow (0.85 + 0.8), so the sizes must reach the file together.
        let toml = STACK3
            .replace("cmd = \"one\"\nsize = 0.5\n", "cmd = \"one\"\n")
            .replace("cmd = \"two\"\nsize = 0.25\n", "cmd = \"two\"\n")
            .replace(
                "cmd = \"three\"\nsize = 0.25\n",
                "cmd = \"three\"\nsize = 0.8\n",
            );
        let (_d, p) = setup(&toml);
        let alone = p.with_file_name("alone.toml");
        std::fs::write(&alone, &toml).unwrap();
        let at = At {
            window: "W",
            group: None,
            tab: Some(("dir", "/tmp/a")),
        };
        edit::set_path(
            &alone,
            at,
            &["split", "pane", "0", "size"],
            Some(0.85.into()),
        )
        .unwrap();
        assert!(
            warden_config::load(&alone).is_err(),
            "one size alone overflows"
        );

        run_sizes(&p, "A", &[0.85, 0.1, 0.05]).unwrap();
        let c = warden_config::load(&p)
            .expect("the saved stack resolves")
            .config;
        assert_eq!(
            panes(&c, "A"),
            vec![
                (0.85, Some("one".into())),
                (0.1, Some("two".into())),
                (0.05, Some("three".into()))
            ]
        );
    }

    #[test]
    fn pane_sizes_keep_to_the_pane_band() {
        let (_d, p) = setup(STACK3);
        run_sizes(&p, "A", &[0.8, 0.175, 0.025]).unwrap();
        let c = warden_config::load(&p).expect("a pane under PANE_SIZE_MIN saves at it");
        let sizes: Vec<f64> = panes(&c.config, "A").into_iter().map(|p| p.0).collect();
        assert_eq!(sizes, vec![0.77, 0.18, 0.05]);
    }

    #[test]
    fn pane_sizes_copy_an_inherited_stack_onto_the_tab() {
        let toml = r##"
[[window]]
title = "W"

[[window.split.pane]]
cmd = "top"
size = 0.5

[[window.split.pane]]
size = 0.5

[[window.tab]]
dir = "/tmp/a"
title = "A"
server = "s"

[[window.tab]]
dir = "/tmp/b"
title = "B"
server = "s"
"##;
        let (_d, p) = setup(toml);
        run_sizes(&p, "A", &[0.7, 0.3]).unwrap();
        let c = warden_config::load(&p).unwrap().config;
        assert_eq!(panes(&c, "A"), vec![(0.7, Some("top".into())), (0.3, None)]);
        assert_eq!(panes(&c, "B"), vec![(0.5, Some("top".into())), (0.5, None)]);
    }

    #[test]
    fn pane_sizes_refused_on_a_count_mismatch() {
        let (_d, p) = setup(STACK3);
        let err = run_sizes(&p, "A", &[0.5, 0.5]).unwrap_err();
        assert_eq!(err, "the column has 2 panes; the config declares 3");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), STACK3);
    }

    #[test]
    fn pane_sizes_on_a_discovered_project() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("proj");
        std::fs::create_dir_all(root.join("p1")).unwrap();
        let toml = format!(
            "[[window]]\ntitle = \"W\"\n\n[[window.root]]\nname = \"R\"\ndir = \"{}\"\nserver = \"s\"\n\n[[window.root.split.pane]]\ncmd = \"x\"\nsize = 0.5\n\n[[window.root.split.pane]]\nsize = 0.5\n",
            root.display()
        );
        let p = d.path().join("config.toml");
        std::fs::write(&p, toml).unwrap();
        let raw = warden_config::raw::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let split = warden_config::resolve::cascade_split_over(
            &raw,
            &raw.windows[0],
            &[raw.windows[0].extra.roots[0].split.as_ref()],
        )
        .unwrap()
        .unwrap();
        let tab = Tab {
            dir: root.join("p1"),
            key: normalize_dir_key(&root.join("p1")),
            group: Some("R".into()),
            split: Some(split),
            ..bare_tab()
        };
        write_pane_sizes(&p, &raw, "W", &tab, &[0.6, 0.4]).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("[[window.root.tab]]"), "{out}");
        let raw = warden_config::raw::parse(&out).unwrap();
        let o = raw.windows[0].extra.roots[0].tabs[0]
            .split
            .as_ref()
            .unwrap();
        let RawSplit::On(t) = o else { panic!("{out}") };
        let got: Vec<f64> = t
            .panes
            .as_ref()
            .unwrap()
            .iter()
            .map(|p| p.size.unwrap())
            .collect();
        assert_eq!(got, vec![0.6, 0.4]);
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
