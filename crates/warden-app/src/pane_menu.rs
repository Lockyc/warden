//! The terminal right-click menu: what a pane offers (Copy, Paste, Split, Close Pane, Scratch
//! Column Width ▸, and on a scratch block Pane Height ▸ or Pane Width ▸), as a pure item list,
//! and the Tauri menu built from it. Each item's id stamps the clicked pane, its window label and
//! tab, `pane-menu:<op>:<pane>:<label>:<tab>` (label and tab escaped — a pop-out's label holds a
//! `:`), so `main.rs::on_menu_event` acts on that pane whichever window is focused when the event
//! lands.

use crate::registry::PaneIdx;
#[cfg(test)]
use crate::registry::COLUMN_OPENING;

/// The fixed choices under Scratch Column Width ▸, the second pane's share, ascending.
pub const PRESETS: [f64; 4] = [0.25, 1.0 / 3.0, 0.5, 2.0 / 3.0];

/// The fixed choices under Pane Height ▸ / Pane Width ▸, a scratch block's share of its run,
/// ascending.
pub const PANE_PRESETS: [f64; 6] = [0.10, 0.15, 0.20, 0.25, 1.0 / 3.0, 0.5];

/// Why a popped-out tab's Split is disabled.
pub const DOCK_TO_SPLIT: &str = "Dock the tab to split";
/// Why a popped-out tab's size submenus are disabled: its divider is the detach page's, which
/// keeps no ratio warden can read.
pub const DOCK_TO_RESIZE: &str = "Dock the tab to resize";
/// Why a column's lone block has no Pane Height ▸: nothing shares the column with it.
pub const ONLY_PANE: &str = "only pane in the column";

/// How close a size must be to another to count as the same one (the file keeps two decimals).
const SAME_SIZE: f64 = 0.005;

const PREFIX: &str = "pane-menu:";

const COLUMN_WIDTH: &str = "Scratch Column Width";

/// What Split does on the clicked pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    /// The tab's second pane, saved to the config (or reopened).
    Split,
    /// A bare shell stacked on the focused block of a Rex scratch column: Split Above (⌘D) or
    /// Split Below.
    Stack,
}

/// What ⌘D is called for `kind`.
pub fn split_title(kind: SplitKind) -> &'static str {
    match kind {
        SplitKind::Split => "Split",
        SplitKind::Stack => "Split Above",
    }
}

/// The tab's split as the menu shows it: the second pane's share now (a divider drag included)
/// and the share it inherits without a `size` of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeState {
    pub current: f64,
    pub inherited: f64,
}

/// The axis a scratch block's run lays its items along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// A stack: the items sit one above another.
    Height,
    /// A side-by-side run.
    Width,
}

impl Axis {
    fn title(self) -> &'static str {
        match self {
            Axis::Height => "Pane Height",
            Axis::Width => "Pane Width",
        }
    }
}

/// A scratch block as the menu shows it: its run's axis and its share of the run now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneSizeState {
    pub axis: Axis,
    pub current: f64,
}

/// What the menu needs to know about the right-clicked pane.
#[derive(Debug, Clone)]
pub struct PaneMenuCtx {
    pub label: String,
    pub tab: String,
    pub pane: PaneIdx,
    pub has_selection: bool,
    /// `Err` is why the tab can't split, shown on the disabled item.
    pub can_split: Result<SplitKind, String>,
    /// `None` while the tab shows no split; `Err` is why Scratch Column Width ▸ is disabled.
    pub split: Option<Result<SizeState, String>>,
    /// Close Pane (`Registry::close_pane_item`): `None` for no item, `Err` why it is disabled.
    pub close_pane: Option<Result<(), String>>,
    /// A scratch block's Pane Height ▸ / Pane Width ▸: `None` on the main pane, `Err` why it is
    /// disabled (shown as Pane Height).
    pub pane_size: Option<Result<PaneSizeState, String>>,
}

/// The menu for pane `pane` of tab `tab` popped out into window `label`: Copy, Paste and Close
/// Pane act on its surface (Close Pane on its scratch pane, while `split_shown`); Split and the
/// size submenus act on a docked tab only.
pub fn detached_ctx(
    label: &str,
    tab: &str,
    pane: PaneIdx,
    has_selection: bool,
    split_shown: bool,
) -> PaneMenuCtx {
    let block = split_shown && matches!(pane, PaneIdx::Block(_));
    PaneMenuCtx {
        label: label.to_owned(),
        tab: tab.to_owned(),
        pane,
        has_selection,
        can_split: Err(DOCK_TO_SPLIT.to_owned()),
        split: split_shown.then(|| Err(DOCK_TO_RESIZE.to_owned())),
        close_pane: block.then_some(Ok(())),
        pane_size: block.then(|| Err(DOCK_TO_RESIZE.to_owned())),
    }
}

/// One menu entry. `checked` is `Some` for a checkable item; `submenu` names the submenu it
/// nests under.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub separator_before: bool,
    pub submenu: Option<&'static str>,
}

/// What an item does, decoded from its id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Copy,
    Paste,
    /// ⌘D: Split, or Split Above in a scratch column.
    Split,
    /// A scratch column's Split Below.
    SplitBelow,
    /// The clicked scratch pane closes.
    ClosePane,
    /// `PRESETS[i]`.
    Size(usize),
    /// Back to the inherited size.
    SizeDefault,
    /// Scratch Column Width ▸ itself, disabled: nothing to choose.
    SizeUnavailable,
    /// The clicked block takes `PANE_PRESETS[i]` of its run.
    PaneSize(usize),
    /// Pane Height ▸ itself, disabled.
    PaneSizeUnavailable,
}

impl Op {
    fn name(self) -> String {
        match self {
            Op::Copy => "copy".into(),
            Op::Paste => "paste".into(),
            Op::Split => "split".into(),
            Op::SplitBelow => "split-below".into(),
            Op::ClosePane => "close".into(),
            Op::Size(i) => format!("size-{i}"),
            Op::SizeDefault => "size-default".into(),
            Op::SizeUnavailable => "size-none".into(),
            Op::PaneSize(i) => format!("pane-size-{i}"),
            Op::PaneSizeUnavailable => "pane-size-none".into(),
        }
    }

    fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "copy" => Op::Copy,
            "paste" => Op::Paste,
            "split" => Op::Split,
            "split-below" => Op::SplitBelow,
            "close" => Op::ClosePane,
            "size-default" => Op::SizeDefault,
            "size-none" => Op::SizeUnavailable,
            "pane-size-none" => Op::PaneSizeUnavailable,
            _ => {
                if let Some(i) = s.strip_prefix("pane-size-") {
                    let i: usize = i.parse().ok()?;
                    return (i < PANE_PRESETS.len()).then_some(Op::PaneSize(i));
                }
                let i: usize = s.strip_prefix("size-")?.parse().ok()?;
                (i < PRESETS.len()).then_some(Op::Size(i))?
            }
        })
    }

    /// An item under Scratch Column Width ▸.
    #[cfg(test)]
    fn is_size(self) -> bool {
        matches!(self, Op::Size(_) | Op::SizeDefault)
    }

    /// A size submenu's disabled stand-in.
    fn is_unavailable(self) -> bool {
        matches!(self, Op::SizeUnavailable | Op::PaneSizeUnavailable)
    }

    /// The menu-bar chord the item mirrors, shown beside it.
    fn accelerator(self) -> Option<&'static str> {
        match self {
            Op::Copy => Some("CmdOrCtrl+C"),
            Op::Paste => Some("CmdOrCtrl+V"),
            Op::Split => Some("CmdOrCtrl+D"),
            Op::ClosePane => Some("CmdOrCtrl+W"),
            _ => None,
        }
    }
}

/// A decoded item id: what to do, on which pane of which tab in which window.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub op: Op,
    pub pane: PaneIdx,
    pub label: String,
    pub tab: String,
}

/// `s` with `%` and `:` percent-escaped, so it holds no field separator.
fn escape(s: &str) -> String {
    s.replace('%', "%25").replace(':', "%3A")
}

/// Inverse of [`escape`]; `None` for a `%` that isn't one of its two escapes.
fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let code = rest.get(i..i + 3)?;
        out.push(match code {
            "%25" => '%',
            "%3A" => ':',
            _ => return None,
        });
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

fn item_id(op: Op, pane: PaneIdx, label: &str, tab: &str) -> String {
    format!(
        "{PREFIX}{}:{}:{}:{}",
        op.name(),
        pane.index(),
        escape(label),
        escape(tab)
    )
}

/// Decode a menu id; `None` for any other menu's id.
pub fn parse(id: &str) -> Option<Target> {
    let mut parts = id.strip_prefix(PREFIX)?.split(':');
    let op = Op::parse(parts.next()?)?;
    let pane = PaneIdx::from_index(parts.next()?.parse().ok()?);
    let label = unescape(parts.next()?)?;
    let tab = unescape(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Target {
        op,
        pane,
        label,
        tab,
    })
}

fn percent(size: f64) -> String {
    format!("{}%", (size * 100.0).round())
}

/// The menu for `ctx`, in order: Copy, Paste — Split, Close Pane (a scratch pane's) — Scratch
/// Column Width ▸, Pane Height ▸ (flattened here; every item under one names it as its
/// `submenu`, and [`build`] nests them).
pub fn items(ctx: &PaneMenuCtx) -> Vec<Item> {
    let item = |op: Op, title: String, enabled: bool, separator_before: bool| Item {
        id: item_id(op, ctx.pane, &ctx.label, &ctx.tab),
        title,
        enabled,
        checked: None,
        separator_before,
        submenu: None,
    };
    let mut out = vec![
        item(Op::Copy, "Copy".into(), ctx.has_selection, false),
        item(Op::Paste, "Paste".into(), true, false),
    ];
    match &ctx.can_split {
        Ok(kind) => {
            out.push(item(Op::Split, split_title(*kind).into(), true, true));
            if *kind == SplitKind::Stack {
                out.push(item(Op::SplitBelow, "Split Below".into(), true, false));
            }
        }
        Err(why) => out.push(item(Op::Split, format!("Split ({why})"), false, true)),
    }
    match &ctx.close_pane {
        None => {}
        Some(Ok(())) => out.push(item(Op::ClosePane, "Close Pane".into(), true, false)),
        Some(Err(why)) => out.push(item(
            Op::ClosePane,
            format!("Close Pane ({why})"),
            false,
            false,
        )),
    }
    match &ctx.split {
        None => {}
        Some(Err(why)) => out.push(item(
            Op::SizeUnavailable,
            format!("{COLUMN_WIDTH} ({why})"),
            false,
            true,
        )),
        Some(Ok(s)) => out.extend(size_items(&ctx.label, &ctx.tab, ctx.pane, *s)),
    }
    let separator = ctx.split.is_none();
    match &ctx.pane_size {
        None => {}
        Some(Err(why)) => out.push(item(
            Op::PaneSizeUnavailable,
            format!("{} ({why})", Axis::Height.title()),
            false,
            separator,
        )),
        Some(Ok(s)) => out.extend(PANE_PRESETS.iter().enumerate().map(|(i, p)| Item {
            checked: Some(same_size(*p, s.current)),
            submenu: Some(s.axis.title()),
            ..item(Op::PaneSize(i), percent(*p), true, separator && i == 0)
        })),
    }
    out
}

fn same_size(a: f64, b: f64) -> bool {
    (a - b).abs() < SAME_SIZE
}

/// Scratch Column Width ▸'s items for a split at `s`, ascending: the presets, the inherited
/// size among them as "(default)", the current one checked. The first carries the separator.
fn size_items(label: &str, tab: &str, pane: PaneIdx, s: SizeState) -> Vec<Item> {
    let mut sizes: Vec<(f64, Op)> = PRESETS
        .iter()
        .enumerate()
        .filter(|(_, p)| !same_size(**p, s.inherited))
        .map(|(i, p)| (*p, Op::Size(i)))
        .collect();
    sizes.push((s.inherited, Op::SizeDefault));
    sizes.sort_by(|a, b| a.0.total_cmp(&b.0));
    sizes
        .into_iter()
        .enumerate()
        .map(|(i, (size, op))| Item {
            id: item_id(op, pane, label, tab),
            title: match op {
                Op::SizeDefault => format!("{} (default)", percent(size)),
                _ => percent(size),
            },
            enabled: true,
            checked: Some(same_size(size, s.current)),
            separator_before: i == 0,
            submenu: Some(COLUMN_WIDTH),
        })
        .collect()
}

/// One entry of the built menu: an item, or a submenu of the items naming it, or a disabled
/// submenu standing in for one with nothing to choose.
enum Node<'a> {
    Item(&'a Item),
    Submenu(&'static str, Vec<&'a Item>),
    Disabled(&'a Item),
}

impl Node<'_> {
    fn separator_before(&self) -> bool {
        match self {
            Node::Item(it) | Node::Disabled(it) => it.separator_before,
            Node::Submenu(_, its) => its.first().is_some_and(|it| it.separator_before),
        }
    }
}

/// `items` as the menu nests them: consecutive items naming the same submenu under it.
fn nodes(items: &[Item]) -> Vec<Node<'_>> {
    let mut out: Vec<Node> = Vec::new();
    for it in items {
        if parse(&it.id).is_some_and(|t| t.op.is_unavailable()) {
            out.push(Node::Disabled(it));
            continue;
        }
        match (it.submenu, out.last_mut()) {
            (Some(title), Some(Node::Submenu(t, its))) if *t == title => its.push(it),
            (Some(title), _) => out.push(Node::Submenu(title, vec![it])),
            (None, _) => out.push(Node::Item(it)),
        }
    }
    out
}

/// The submenus [`build`] makes of `items`: each title and its items' titles (none for a
/// disabled one).
#[cfg(test)]
fn submenus(items: &[Item]) -> Vec<(String, Vec<String>)> {
    nodes(items)
        .into_iter()
        .filter_map(|n| match n {
            Node::Item(_) => None,
            Node::Submenu(t, its) => {
                Some((t.to_owned(), its.iter().map(|i| i.title.clone()).collect()))
            }
            Node::Disabled(it) => Some((it.title.clone(), Vec::new())),
        })
        .collect()
}

/// One item as a Tauri menu entry: checkable when `checked` is set, else with its chord.
fn entry<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    it: &Item,
) -> tauri::Result<Box<dyn tauri::menu::IsMenuItem<R>>> {
    use tauri::menu::{CheckMenuItemBuilder, MenuItemBuilder};
    Ok(match it.checked {
        Some(checked) => Box::new(
            CheckMenuItemBuilder::with_id(&it.id, &it.title)
                .enabled(it.enabled)
                .checked(checked)
                .build(app)?,
        ),
        None => {
            let mut b = MenuItemBuilder::with_id(&it.id, &it.title).enabled(it.enabled);
            if let Some(accel) = parse(&it.id).and_then(|t| t.op.accelerator()) {
                b = b.accelerator(accel);
            }
            Box::new(b.build(app)?)
        }
    })
}

/// The Tauri menu for `items`, nested as [`nodes`] groups them.
pub fn build<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    items: &[Item],
) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{Menu, PredefinedMenuItem, SubmenuBuilder};
    let menu = Menu::new(app)?;
    for node in nodes(items) {
        if node.separator_before() {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        match node {
            Node::Item(it) => menu.append(&*entry(app, it)?)?,
            Node::Disabled(it) => {
                menu.append(&SubmenuBuilder::new(app, &it.title).enabled(false).build()?)?
            }
            Node::Submenu(title, its) => {
                let mut sub = SubmenuBuilder::new(app, title);
                for it in its {
                    sub = sub.item(&*entry(app, it)?);
                }
                menu.append(&sub.build()?)?;
            }
        }
    }
    Ok(menu)
}

/// Pop `ctx`'s menu over its window at `(x, y)` (content-view points, top-left origin).
pub fn popup(app: &tauri::AppHandle, ctx: &PaneMenuCtx, x: f64, y: f64) -> Result<(), String> {
    use tauri::Manager;
    let menu = build(app, &items(ctx)).map_err(|e| e.to_string())?;
    let window = app.get_window(&ctx.label).ok_or("window not found")?;
    window
        .popup_menu_at(&menu, tauri::LogicalPosition::new(x, y))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PaneMenuCtx {
        PaneMenuCtx {
            label: "w0".into(),
            tab: "/tmp/a".into(),
            pane: PaneIdx::Primary,
            has_selection: true,
            can_split: Ok(SplitKind::Split),
            split: None,
            close_pane: None,
            pane_size: None,
        }
    }

    fn sized(current: f64, inherited: f64) -> PaneMenuCtx {
        PaneMenuCtx {
            split: Some(Ok(SizeState { current, inherited })),
            ..ctx()
        }
    }

    fn op_of(i: &Item) -> Op {
        parse(&i.id).unwrap().op
    }

    fn find(items: &[Item], op: Op) -> &Item {
        items.iter().find(|i| op_of(i) == op).unwrap()
    }

    fn sizes(items: &[Item]) -> Vec<(String, Option<bool>)> {
        items
            .iter()
            .filter(|i| op_of(i).is_size())
            .map(|i| (i.title.clone(), i.checked))
            .collect()
    }

    #[test]
    fn copy_disabled_without_selection() {
        let on = items(&ctx());
        assert!(find(&on, Op::Copy).enabled);
        let off = items(&PaneMenuCtx {
            has_selection: false,
            ..ctx()
        });
        assert!(!find(&off, Op::Copy).enabled);
        assert!(find(&off, Op::Paste).enabled);
        assert_eq!(
            off.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            ["Copy", "Paste", "Split"]
        );
    }

    #[test]
    fn rex_block_offers_split_above_and_below() {
        let m = items(&PaneMenuCtx {
            pane: PaneIdx::Block(0),
            can_split: Ok(SplitKind::Stack),
            ..ctx()
        });
        let s = find(&m, Op::Split);
        assert_eq!((s.title.as_str(), s.enabled), ("Split Above", true));
        assert!(s.separator_before);
        let b = find(&m, Op::SplitBelow);
        assert_eq!((b.title.as_str(), b.enabled), ("Split Below", true));
        assert!(!b.separator_before);
        assert_eq!(parse(&b.id).unwrap().pane, PaneIdx::Block(0));
        // A plain tab has one Split.
        assert!(items(&ctx()).iter().all(|i| op_of(i) != Op::SplitBelow));
    }

    fn block(pane_size: Option<Result<PaneSizeState, String>>) -> PaneMenuCtx {
        PaneMenuCtx {
            pane: PaneIdx::Block(1),
            can_split: Ok(SplitKind::Stack),
            close_pane: Some(Ok(())),
            pane_size,
            ..sized(0.5, 0.5)
        }
    }

    fn pane_sizes(items: &[Item]) -> Vec<(String, Option<bool>)> {
        items
            .iter()
            .filter(|i| matches!(op_of(i), Op::PaneSize(_)))
            .map(|i| (i.title.clone(), i.checked))
            .collect()
    }

    #[test]
    fn tab_split_submenu_is_scratch_column_width() {
        let s = submenus(&items(&sized(0.5, 0.5)));
        assert_eq!(
            s,
            [(
                "Scratch Column Width".to_string(),
                vec![
                    "25%".to_string(),
                    "33%".into(),
                    "50% (default)".into(),
                    "67%".into()
                ]
            )]
        );
        // The main pane offers no Pane Height.
        assert!(items(&sized(0.5, 0.5))
            .iter()
            .all(|i| !matches!(op_of(i), Op::PaneSize(_) | Op::PaneSizeUnavailable)));
    }

    #[test]
    fn block_offers_pane_height_presets_with_current_checked() {
        let m = items(&block(Some(Ok(PaneSizeState {
            axis: Axis::Height,
            current: 0.2,
        }))));
        let s = submenus(&m);
        assert_eq!(
            s.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["Scratch Column Width", "Pane Height"]
        );
        assert_eq!(
            s[1].1,
            ["10%", "15%", "20%", "25%", "33%", "50%"].map(String::from)
        );
        let checked: Vec<_> = pane_sizes(&m)
            .into_iter()
            .filter(|(_, c)| *c == Some(true))
            .map(|(t, _)| t)
            .collect();
        assert_eq!(checked, ["20%"]);
        let twenty = m.iter().find(|i| i.title == "20%").unwrap();
        let t = parse(&twenty.id).unwrap();
        assert_eq!((t.op, t.pane), (Op::PaneSize(2), PaneIdx::Block(1)));
    }

    #[test]
    fn side_by_side_block_offers_pane_width() {
        let m = items(&block(Some(Ok(PaneSizeState {
            axis: Axis::Width,
            current: 0.5,
        }))));
        let s = submenus(&m);
        assert_eq!(s[1].0, "Pane Width");
        assert_eq!(
            pane_sizes(&m)
                .into_iter()
                .filter(|(_, c)| *c == Some(true))
                .map(|(t, _)| t)
                .collect::<Vec<_>>(),
            ["50%"]
        );
    }

    #[test]
    fn only_pane_is_disabled_with_the_reason() {
        let m = items(&block(Some(Err(ONLY_PANE.to_owned()))));
        let p = find(&m, Op::PaneSizeUnavailable);
        assert_eq!(
            (p.title.as_str(), p.enabled),
            ("Pane Height (only pane in the column)", false)
        );
        assert!(pane_sizes(&m).is_empty());
        assert_eq!(
            submenus(&m)[1],
            ("Pane Height (only pane in the column)".to_string(), vec![])
        );
    }

    #[test]
    fn pop_out_disables_both_with_dock_to_resize() {
        let m = items(&detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Block(0),
            false,
            true,
        ));
        assert_eq!(
            submenus(&m),
            [
                (
                    "Scratch Column Width (Dock the tab to resize)".to_string(),
                    vec![]
                ),
                ("Pane Height (Dock the tab to resize)".to_string(), vec![]),
            ]
        );
        // Its main pane has only the column's.
        let m = items(&detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Primary,
            false,
            true,
        ));
        assert!(m.iter().all(|i| op_of(i) != Op::PaneSizeUnavailable));
    }

    #[test]
    fn close_pane_only_in_the_scratch_column() {
        assert!(items(&sized(0.5, 0.5))
            .iter()
            .all(|i| op_of(i) != Op::ClosePane));
        let m = items(&PaneMenuCtx {
            pane: PaneIdx::Block(1),
            can_split: Ok(SplitKind::Stack),
            close_pane: Some(Ok(())),
            ..sized(0.5, 0.5)
        });
        assert_eq!(
            m.iter()
                .map(|i| i.title.as_str())
                .take(6)
                .collect::<Vec<_>>(),
            [
                "Copy",
                "Paste",
                "Split Above",
                "Split Below",
                "Close Pane",
                "25%"
            ]
        );
        let c = find(&m, Op::ClosePane);
        assert!(c.enabled && !c.separator_before);
        assert_eq!(parse(&c.id).unwrap().pane, PaneIdx::Block(1));
        // A pop-out's scratch pane closes too.
        let d = items(&detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Block(0),
            false,
            true,
        ));
        assert!(find(&d, Op::ClosePane).enabled);
        // Its main pane has none.
        let d = items(&detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Primary,
            false,
            true,
        ));
        assert!(d.iter().all(|i| op_of(i) != Op::ClosePane));
    }

    /// While the column's open is in flight there is no block to close yet.
    #[test]
    fn close_pane_disabled_with_the_reason_while_the_column_opens() {
        let m = items(&PaneMenuCtx {
            pane: PaneIdx::Block(0),
            close_pane: Some(Err(COLUMN_OPENING.to_owned())),
            ..ctx()
        });
        let c = find(&m, Op::ClosePane);
        assert_eq!(
            (c.title.as_str(), c.enabled),
            ("Close Pane (Scratch column is still opening)", false)
        );
    }

    #[test]
    fn sidebar_rex_session_split_disabled_with_reason() {
        let m = items(&PaneMenuCtx {
            can_split: Err("a Rex session opened from the sidebar can't split".into()),
            ..ctx()
        });
        let s = find(&m, Op::Split);
        assert!(!s.enabled);
        assert_eq!(
            s.title,
            "Split (a Rex session opened from the sidebar can't split)"
        );
    }

    #[test]
    fn size_lists_presets_plus_inherited_default() {
        let m = items(&sized(0.3, 0.3));
        assert_eq!(
            sizes(&m).into_iter().map(|(t, _)| t).collect::<Vec<_>>(),
            ["25%", "30% (default)", "33%", "50%", "67%"]
        );
        assert_eq!(find(&m, Op::SizeDefault).title, "30% (default)");
        let first = m.iter().find(|i| op_of(i).is_size()).unwrap();
        assert!(first.separator_before);

        // An inherited size that is a preset takes that preset's place.
        let m = items(&sized(0.5, 0.5));
        assert_eq!(
            sizes(&m).into_iter().map(|(t, _)| t).collect::<Vec<_>>(),
            ["25%", "33%", "50% (default)", "67%"]
        );
    }

    #[test]
    fn current_size_checked() {
        assert_eq!(
            sizes(&items(&sized(0.334, 0.5))),
            [
                ("25%".to_string(), Some(false)),
                ("33%".to_string(), Some(true)),
                ("50% (default)".to_string(), Some(false)),
                ("67%".to_string(), Some(false)),
            ]
        );
        // A drag between presets ticks none.
        assert!(sizes(&items(&sized(0.27, 0.5)))
            .iter()
            .all(|(_, c)| *c == Some(false)));
    }

    #[test]
    fn no_size_section_when_unsplit() {
        let m = items(&ctx());
        assert!(m.iter().all(|i| !matches!(
            op_of(i),
            Op::Size(_) | Op::SizeDefault | Op::SizeUnavailable
        )));
        assert!(m.iter().all(|i| i.checked.is_none()));
    }

    #[test]
    fn size_without_a_config_entry_is_disabled_with_the_reason() {
        let m = items(&PaneMenuCtx {
            split: Some(Err("no config entry for tab \"x\"".into())),
            ..ctx()
        });
        let s = find(&m, Op::SizeUnavailable);
        assert_eq!(
            (s.title.as_str(), s.enabled, s.separator_before),
            (
                "Scratch Column Width (no config entry for tab \"x\")",
                false,
                true
            )
        );
        assert!(sizes(&m).is_empty());
    }

    #[test]
    fn pop_out_offers_copy_paste_and_dock_reasons() {
        let c = detached_ctx("shell-detach:k1", "/tmp/a", PaneIdx::Block(0), false, true);
        let m = items(&c);
        assert!(!find(&m, Op::Copy).enabled);
        assert!(find(&m, Op::Paste).enabled);
        let split = find(&m, Op::Split);
        assert_eq!(
            (split.title.as_str(), split.enabled),
            ("Split (Dock the tab to split)", false)
        );
        let size = find(&m, Op::SizeUnavailable);
        assert_eq!(
            (size.title.as_str(), size.enabled),
            ("Scratch Column Width (Dock the tab to resize)", false)
        );
        // An unsplit pop-out has no size submenu at all.
        let m = items(&detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Primary,
            true,
            false,
        ));
        assert!(m.iter().all(|i| op_of(i) != Op::SizeUnavailable));
    }

    #[test]
    fn ids_round_trip_a_pop_out_label_and_a_colon_in_the_tab() {
        let m = items(&PaneMenuCtx {
            label: "shell-detach:k%1".into(),
            tab: "/tmp/a:b".into(),
            pane: PaneIdx::Block(2),
            close_pane: Some(Ok(())),
            ..sized(0.5, 0.3)
        });
        let ops: Vec<Op> = m
            .iter()
            .map(|i| {
                let t = parse(&i.id).unwrap();
                assert_eq!(
                    (t.pane, t.label.as_str(), t.tab.as_str()),
                    (PaneIdx::Block(2), "shell-detach:k%1", "/tmp/a:b")
                );
                t.op
            })
            .collect();
        assert_eq!(
            ops,
            [
                Op::Copy,
                Op::Paste,
                Op::Split,
                Op::ClosePane,
                Op::Size(0),
                Op::SizeDefault,
                Op::Size(1),
                Op::Size(2),
                Op::Size(3)
            ]
        );
        assert_eq!(parse("pane-menu:size-9:0:w0:t"), None);
        assert_eq!(parse("pane-menu:pane-size-6:1:w0:t"), None);
        assert_eq!(parse("warden:split-pane"), None);
    }
}
