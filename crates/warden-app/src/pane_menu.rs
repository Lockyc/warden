//! The terminal right-click menu: what a pane offers (Copy, Paste, Split, Size ▸), as a pure
//! item list, and the Tauri menu built from it. Each item's id stamps its window label and tab,
//! `pane-menu:<op>:<label>:<tab>`, so `main.rs::on_menu_event` acts on the clicked pane's tab
//! whichever window is focused when the event lands.

use crate::registry::PaneIdx;

/// The fixed choices under Size ▸, the second pane's share, ascending.
pub const PRESETS: [f64; 4] = [0.25, 1.0 / 3.0, 0.5, 2.0 / 3.0];

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
    /// The clicked pane. No item depends on it yet; a scratch-column-only item will.
    #[allow(dead_code)]
    pub pane: PaneIdx,
    pub has_selection: bool,
    /// `Err` is why the tab can't split, shown on the disabled item.
    pub can_split: Result<SplitKind, String>,
    /// `None` while the tab shows no split.
    pub split: Option<SizeState>,
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
    /// `PRESETS[i]`.
    Size(usize),
    /// Back to the inherited size.
    SizeDefault,
}

impl Op {
    fn name(self) -> String {
        match self {
            Op::Copy => "copy".into(),
            Op::Paste => "paste".into(),
            Op::Split => "split".into(),
            Op::Size(i) => format!("size-{i}"),
            Op::SizeDefault => "size-default".into(),
        }
    }

    fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "copy" => Op::Copy,
            "paste" => Op::Paste,
            "split" => Op::Split,
            "size-default" => Op::SizeDefault,
            _ => {
                let i: usize = s.strip_prefix("size-")?.parse().ok()?;
                (i < PRESETS.len()).then_some(Op::Size(i))?
            }
        })
    }

    fn is_size(self) -> bool {
        matches!(self, Op::Size(_) | Op::SizeDefault)
    }

    /// The menu-bar chord the item mirrors, shown beside it.
    fn accelerator(self) -> Option<&'static str> {
        match self {
            Op::Copy => Some("CmdOrCtrl+C"),
            Op::Paste => Some("CmdOrCtrl+V"),
            Op::Split => Some("CmdOrCtrl+D"),
            _ => None,
        }
    }
}

/// The id of item `op` for tab `tab` in window `label`.
fn item_id(op: Op, label: &str, tab: &str) -> String {
    format!("{PREFIX}{}:{label}:{tab}", op.name())
}

/// Decode a menu id into `(op, label, tab)`; `None` for any other menu's id. The tab id is the
/// rest of the id, so one holding a `:` (a path) survives.
pub fn parse(id: &str) -> Option<(Op, String, String)> {
    let mut parts = id.strip_prefix(PREFIX)?.splitn(3, ':');
    let op = Op::parse(parts.next()?)?;
    let label = parts.next()?;
    let tab = parts.next()?;
    Some((op, label.to_owned(), tab.to_owned()))
}

fn percent(size: f64) -> String {
    format!("{}%", (size * 100.0).round())
}

/// The menu for `ctx`, in order: Copy, Paste — Split — Size ▸ (flattened here; every size item
/// carries a size op, and [`build`] nests them).
pub fn items(ctx: &PaneMenuCtx) -> Vec<Item> {
    let item = |op: Op, title: String, enabled: bool, separator_before: bool| Item {
        id: item_id(op, &ctx.label, &ctx.tab),
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
    if let Some(s) = ctx.split {
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
        let op = parse(&it.id).map(|(op, ..)| op);
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
            if size.is_none() && it.separator_before {
                menu.append(&PredefinedMenuItem::separator(app)?)?;
            }
            size = Some(
                size.unwrap_or_else(|| SubmenuBuilder::new(app, "Size"))
                    .item(&*entry),
            );
            continue;
        }
        if it.separator_before {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        menu.append(&*entry)?;
    }
    if let Some(size) = size {
        menu.append(&size.build()?)?;
    }
    Ok(menu)
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
        }
    }

    fn find<'a>(items: &'a [Item], op: &str) -> &'a Item {
        let id = format!("pane-menu:{op}:w0:/tmp/a");
        items.iter().find(|i| i.id == id).unwrap()
    }

    fn sizes(items: &[Item]) -> Vec<(String, Option<bool>)> {
        items
            .iter()
            .filter(|i| i.id.starts_with("pane-menu:size"))
            .map(|i| (i.title.clone(), i.checked))
            .collect()
    }

    #[test]
    fn copy_disabled_without_selection() {
        let on = items(&ctx());
        assert!(find(&on, "copy").enabled);
        let off = items(&PaneMenuCtx {
            has_selection: false,
            ..ctx()
        });
        assert!(!find(&off, "copy").enabled);
        assert!(find(&off, "paste").enabled);
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
        let s = find(&m, "split");
        assert_eq!((s.title.as_str(), s.enabled), ("Split Below", true));
        assert!(s.separator_before);
    }

    #[test]
    fn sidebar_rex_session_split_disabled_with_reason() {
        let m = items(&PaneMenuCtx {
            can_split: Err("a Rex session opened from the sidebar can't split".into()),
            ..ctx()
        });
        let s = find(&m, "split");
        assert!(!s.enabled);
        assert_eq!(
            s.title,
            "Split (a Rex session opened from the sidebar can't split)"
        );
    }

    #[test]
    fn size_lists_presets_plus_inherited_default() {
        let m = items(&PaneMenuCtx {
            split: Some(SizeState {
                current: 0.3,
                inherited: 0.3,
            }),
            ..ctx()
        });
        assert_eq!(
            sizes(&m).into_iter().map(|(t, _)| t).collect::<Vec<_>>(),
            ["25%", "30% (default)", "33%", "50%", "67%"]
        );
        assert_eq!(find(&m, "size-default").title, "30% (default)");
        let first = m
            .iter()
            .find(|i| i.id.starts_with("pane-menu:size"))
            .unwrap();
        assert!(first.separator_before);

        // An inherited size that is a preset takes that preset's place.
        let m = items(&PaneMenuCtx {
            split: Some(SizeState {
                current: 0.5,
                inherited: 0.5,
            }),
            ..ctx()
        });
        assert_eq!(
            sizes(&m).into_iter().map(|(t, _)| t).collect::<Vec<_>>(),
            ["25%", "33%", "50% (default)", "67%"]
        );
    }

    #[test]
    fn current_size_checked() {
        let m = items(&PaneMenuCtx {
            split: Some(SizeState {
                current: 0.334,
                inherited: 0.5,
            }),
            ..ctx()
        });
        assert_eq!(
            sizes(&m),
            [
                ("25%".to_string(), Some(false)),
                ("33%".to_string(), Some(true)),
                ("50% (default)".to_string(), Some(false)),
                ("67%".to_string(), Some(false)),
            ]
        );
        // A drag between presets ticks none.
        let m = items(&PaneMenuCtx {
            split: Some(SizeState {
                current: 0.27,
                inherited: 0.5,
            }),
            ..ctx()
        });
        assert!(sizes(&m).iter().all(|(_, c)| *c == Some(false)));
    }

    #[test]
    fn no_size_section_when_unsplit() {
        let m = items(&ctx());
        assert!(sizes(&m).is_empty());
        assert!(m.iter().all(|i| i.checked.is_none()));
    }

    #[test]
    fn ids_round_trip_with_a_colon_in_the_tab() {
        let m = items(&PaneMenuCtx {
            tab: "/tmp/a:b".into(),
            split: Some(SizeState {
                current: 0.5,
                inherited: 0.3,
            }),
            ..ctx()
        });
        let ops: Vec<Op> = m
            .iter()
            .map(|i| {
                let (op, label, tab) = parse(&i.id).unwrap();
                assert_eq!((label.as_str(), tab.as_str()), ("w0", "/tmp/a:b"));
                op
            })
            .collect();
        assert_eq!(
            ops,
            [
                Op::Copy,
                Op::Paste,
                Op::Split,
                Op::Size(0),
                Op::SizeDefault,
                Op::Size(1),
                Op::Size(2),
                Op::Size(3)
            ]
        );
        assert_eq!(parse("pane-menu:size-9:w0:t"), None);
        assert_eq!(parse("warden:split-pane"), None);
    }
}
