//! The terminal right-click menu: what a pane offers (Copy, Paste, Split, Close Pane, Size ▸), as a pure
//! item list, and the Tauri menu built from it. Each item's id stamps the clicked pane, its
//! window label and tab, `pane-menu:<op>:<pane>:<label>:<tab>` (label and tab escaped — a
//! pop-out's label holds a `:`), so `main.rs::on_menu_event` acts on that pane whichever window
//! is focused when the event lands.

use crate::registry::{CloseTarget, PaneIdx, COLUMN_OPENING};

/// The fixed choices under Size ▸, the second pane's share, ascending.
pub const PRESETS: [f64; 4] = [0.25, 1.0 / 3.0, 0.5, 2.0 / 3.0];

/// Why a popped-out tab's Split is disabled.
pub const DOCK_TO_SPLIT: &str = "Dock the tab to split";
/// Why a popped-out tab's Size ▸ is disabled: its divider is the detach page's, which keeps no
/// ratio warden can read.
pub const DOCK_TO_RESIZE: &str = "Dock the tab to resize";

/// How close a size must be to another to count as the same one (the file keeps two decimals).
const SAME_SIZE: f64 = 0.005;

const PREFIX: &str = "pane-menu:";

/// What Split does on the clicked pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    /// The tab's second pane, saved to the config (or reopened).
    Split,
    /// A bare shell stacked below the focused block of a Rex scratch column.
    SplitBelow,
}

/// The tab's split as the menu shows it: the second pane's share now (a divider drag included)
/// and the share it inherits without a `size` of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeState {
    pub current: f64,
    pub inherited: f64,
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
    /// `None` while the tab shows no split; `Err` is why Size ▸ is disabled.
    pub split: Option<Result<SizeState, String>>,
    /// Close Pane ([`close_pane_of`]): `None` for no item, `Err` why it is disabled.
    pub close_pane: Option<Result<(), String>>,
}

/// Close Pane as ⌘W's target for the clicked pane decides it (a right-click focuses the pane):
/// offered when ⌘W would close a scratch block, disabled with the reason while there is no block
/// to close yet (`None`), absent when ⌘W closes an agent or the project tab instead.
pub fn close_pane_of(target: Option<CloseTarget>) -> Option<Result<(), String>> {
    match target {
        None => Some(Err(COLUMN_OPENING.to_owned())),
        Some(CloseTarget::Block { .. }) => Some(Ok(())),
        Some(CloseTarget::Agent | CloseTarget::Project) => None,
    }
}

/// The menu for pane `pane` of tab `tab` popped out into window `label`: Copy, Paste and Close
/// Pane act on its surface; Split and Size ▸ act on a docked tab only. `close` is the pop-out's
/// ⌘W target.
pub fn detached_ctx(
    label: &str,
    tab: &str,
    pane: PaneIdx,
    has_selection: bool,
    split_shown: bool,
    close: CloseTarget,
) -> PaneMenuCtx {
    PaneMenuCtx {
        label: label.to_owned(),
        tab: tab.to_owned(),
        pane,
        has_selection,
        can_split: Err(DOCK_TO_SPLIT.to_owned()),
        split: split_shown.then(|| Err(DOCK_TO_RESIZE.to_owned())),
        close_pane: close_pane_of(Some(close)),
    }
}

/// One menu entry. `checked` is `Some` for a checkable item.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub separator_before: bool,
}

/// What an item does, decoded from its id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Copy,
    Paste,
    Split,
    /// The clicked scratch pane closes, as ⌘W closes the focused one.
    ClosePane,
    /// `PRESETS[i]`.
    Size(usize),
    /// Back to the inherited size.
    SizeDefault,
    /// Size ▸ itself, disabled: nothing to choose.
    SizeUnavailable,
}

impl Op {
    fn name(self) -> String {
        match self {
            Op::Copy => "copy".into(),
            Op::Paste => "paste".into(),
            Op::Split => "split".into(),
            Op::ClosePane => "close".into(),
            Op::Size(i) => format!("size-{i}"),
            Op::SizeDefault => "size-default".into(),
            Op::SizeUnavailable => "size-none".into(),
        }
    }

    fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "copy" => Op::Copy,
            "paste" => Op::Paste,
            "split" => Op::Split,
            "close" => Op::ClosePane,
            "size-default" => Op::SizeDefault,
            "size-none" => Op::SizeUnavailable,
            _ => {
                let i: usize = s.strip_prefix("size-")?.parse().ok()?;
                (i < PRESETS.len()).then_some(Op::Size(i))?
            }
        })
    }

    /// An item under Size ▸.
    fn is_size(self) -> bool {
        matches!(self, Op::Size(_) | Op::SizeDefault)
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

/// The menu for `ctx`, in order: Copy, Paste — Split, Close Pane (a scratch pane's) — Size ▸
/// (flattened here; every item under it carries a size op, and [`build`] nests them).
pub fn items(ctx: &PaneMenuCtx) -> Vec<Item> {
    let item = |op: Op, title: String, enabled: bool, separator_before: bool| Item {
        id: item_id(op, ctx.pane, &ctx.label, &ctx.tab),
        title,
        enabled,
        checked: None,
        separator_before,
    };
    let mut out = vec![
        item(Op::Copy, "Copy".into(), ctx.has_selection, false),
        item(Op::Paste, "Paste".into(), true, false),
    ];
    out.push(match &ctx.can_split {
        Ok(SplitKind::Split) => item(Op::Split, "Split".into(), true, true),
        Ok(SplitKind::SplitBelow) => item(Op::Split, "Split Below".into(), true, true),
        Err(why) => item(Op::Split, format!("Split ({why})"), false, true),
    });
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
            format!("Size ({why})"),
            false,
            true,
        )),
        Some(Ok(s)) => {
            let same = |a: f64, b: f64| (a - b).abs() < SAME_SIZE;
            let mut sizes: Vec<(f64, Op)> = PRESETS
                .iter()
                .enumerate()
                .filter(|(_, p)| !same(**p, s.inherited))
                .map(|(i, p)| (*p, Op::Size(i)))
                .collect();
            sizes.push((s.inherited, Op::SizeDefault));
            sizes.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (i, (size, op)) in sizes.into_iter().enumerate() {
                let title = match op {
                    Op::SizeDefault => format!("{} (default)", percent(size)),
                    _ => percent(size),
                };
                out.push(Item {
                    checked: Some(same(size, s.current)),
                    ..item(op, title, true, i == 0)
                });
            }
        }
    }
    out
}

/// The Tauri menu for `items`, the size items nested under Size ▸.
pub fn build<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    items: &[Item],
) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{
        CheckMenuItemBuilder, IsMenuItem, Menu, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
    };
    let menu = Menu::new(app)?;
    let mut size: Option<SubmenuBuilder<R, _>> = None;
    for it in items {
        let op = parse(&it.id).map(|t| t.op);
        if it.separator_before && !(size.is_some() && op.is_some_and(Op::is_size)) {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        if op == Some(Op::SizeUnavailable) {
            menu.append(&SubmenuBuilder::new(app, &it.title).enabled(false).build()?)?;
            continue;
        }
        let entry: Box<dyn IsMenuItem<R>> = match it.checked {
            Some(checked) => Box::new(
                CheckMenuItemBuilder::with_id(&it.id, &it.title)
                    .enabled(it.enabled)
                    .checked(checked)
                    .build(app)?,
            ),
            None => {
                let mut b = MenuItemBuilder::with_id(&it.id, &it.title).enabled(it.enabled);
                if let Some(accel) = op.and_then(Op::accelerator) {
                    b = b.accelerator(accel);
                }
                Box::new(b.build(app)?)
            }
        };
        if op.is_some_and(Op::is_size) {
            size = Some(
                size.unwrap_or_else(|| SubmenuBuilder::new(app, "Size"))
                    .item(&*entry),
            );
            continue;
        }
        menu.append(&*entry)?;
    }
    if let Some(size) = size {
        menu.append(&size.build()?)?;
    }
    Ok(menu)
}

/// Pop `ctx`'s menu over its window at `(x, y)` (content-view points, top-left origin).
pub fn popup(app: &tauri::AppHandle, ctx: &PaneMenuCtx, x: f64, y: f64) -> Result<(), String> {
    use tauri::Manager;
    let window = app.get_window(&ctx.label).ok_or("window not found")?;
    let menu = build(app, &items(ctx)).map_err(|e| e.to_string())?;
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
    fn rex_block_offers_split_below() {
        let m = items(&PaneMenuCtx {
            pane: PaneIdx::Block(0),
            can_split: Ok(SplitKind::SplitBelow),
            ..ctx()
        });
        let s = find(&m, Op::Split);
        assert_eq!((s.title.as_str(), s.enabled), ("Split Below", true));
        assert!(s.separator_before);
    }

    #[test]
    fn close_pane_only_in_the_scratch_column() {
        assert!(items(&sized(0.5, 0.5))
            .iter()
            .all(|i| op_of(i) != Op::ClosePane));
        assert_eq!(close_pane_of(Some(CloseTarget::Project)), None);
        assert_eq!(close_pane_of(Some(CloseTarget::Agent)), None);
        let m = items(&PaneMenuCtx {
            pane: PaneIdx::Block(1),
            can_split: Ok(SplitKind::SplitBelow),
            close_pane: close_pane_of(Some(CloseTarget::Block {
                tab: "/tmp/a".into(),
                block: 1,
            })),
            ..sized(0.5, 0.5)
        });
        assert_eq!(
            m.iter()
                .map(|i| i.title.as_str())
                .take(5)
                .collect::<Vec<_>>(),
            ["Copy", "Paste", "Split Below", "Close Pane", "25%"]
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
            CloseTarget::Block {
                tab: "/tmp/a".into(),
                block: 0,
            },
        ));
        assert!(find(&d, Op::ClosePane).enabled);
    }

    /// While the column's open is in flight ⌘W has no block to close, and neither does the item.
    #[test]
    fn close_pane_disabled_with_the_reason_while_the_column_opens() {
        let m = items(&PaneMenuCtx {
            pane: PaneIdx::Block(0),
            close_pane: close_pane_of(None),
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
            ("Size (no config entry for tab \"x\")", false, true)
        );
        assert!(sizes(&m).is_empty());
    }

    #[test]
    fn pop_out_offers_copy_paste_and_dock_reasons() {
        let c = detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Block(0),
            false,
            true,
            CloseTarget::Project,
        );
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
            ("Size (Dock the tab to resize)", false)
        );
        // An unsplit pop-out has no Size ▸ at all.
        let m = items(&detached_ctx(
            "shell-detach:k1",
            "/tmp/a",
            PaneIdx::Primary,
            true,
            false,
            CloseTarget::Project,
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
        assert_eq!(parse("warden:split-pane"), None);
    }
}
