//! Rex's view layout as warden sees it: a split tree of blocks, flattened to tiles and
//! dividers for the page, and the inverse direction — a config pane stack as the layout a
//! new Rex session is created with.

// No caller yet: the column wiring lands in the following tasks, which remove this allow.
#![allow(dead_code)]

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};
use warden_config::StackPane;

use super::{terminal_options, TERMINAL_FLAVOR};

/// A rectangle in fractions of the window.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Frac {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Block {
        block_id: String,
        rect: Frac,
    },
    Split {
        split_id: String,
        dir: Dir,
        ratio: f64,
        rect: Frac,
        before: Box<Layout>,
        after: Box<Layout>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tile {
    pub block_id: String,
    pub rect: Frac,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Divider {
    pub split_id: String,
    pub dir: Dir,
    pub ratio: f64,
    /// The split's own rect.
    pub rect: Frac,
    /// First leaf of each side, for `session.resize_pane`.
    pub before_block: String,
    pub after_block: String,
}

fn parse_rect(v: &Value) -> Option<Frac> {
    let a = v.as_array()?;
    if a.len() != 4 {
        return None;
    }
    let n = |i: usize| a[i].as_f64();
    Some(Frac {
        x: n(0)?,
        y: n(1)?,
        w: n(2)?,
        h: n(3)?,
    })
}

impl Layout {
    /// Parse a `layers[0].layout` node; `None` on null or malformed input.
    pub fn parse(node: &Value) -> Option<Layout> {
        let rect = parse_rect(node.get("rect")?)?;
        if let Some(id) = node.get("block_id").and_then(Value::as_str) {
            return Some(Layout::Block {
                block_id: id.to_owned(),
                rect,
            });
        }
        let s = node.get("split")?;
        // Rex: an unknown direction is horizontal.
        let dir = match s.get("direction").and_then(Value::as_str) {
            Some("vertical") => Dir::Vertical,
            _ => Dir::Horizontal,
        };
        Some(Layout::Split {
            split_id: s.get("split_id")?.as_str()?.to_owned(),
            dir,
            ratio: s.get("ratio")?.as_f64()?,
            rect,
            before: Box::new(Layout::parse(s.get("before")?)?),
            after: Box::new(Layout::parse(s.get("after")?)?),
        })
    }

    /// A full-rect block: a plain tab, or a fallback before Rex has reported a layout.
    pub fn single(block_id: &str) -> Layout {
        Layout::Block {
            block_id: block_id.to_owned(),
            rect: Frac {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
        }
    }

    /// Every block's rect, in layout order.
    pub fn tiles(&self) -> Vec<Tile> {
        match self {
            Layout::Block { block_id, rect } => vec![Tile {
                block_id: block_id.clone(),
                rect: *rect,
            }],
            Layout::Split { before, after, .. } => {
                let mut t = before.tiles();
                t.extend(after.tiles());
                t
            }
        }
    }

    /// Every split, outermost first.
    pub fn dividers(&self) -> Vec<Divider> {
        match self {
            Layout::Block { .. } => Vec::new(),
            Layout::Split {
                split_id,
                dir,
                ratio,
                rect,
                before,
                after,
            } => {
                let mut d = vec![Divider {
                    split_id: split_id.clone(),
                    dir: *dir,
                    ratio: *ratio,
                    rect: *rect,
                    before_block: before.first_block().to_owned(),
                    after_block: after.first_block().to_owned(),
                }];
                d.extend(before.dividers());
                d.extend(after.dividers());
                d
            }
        }
    }

    /// Block ids in layout order.
    pub fn blocks(&self) -> Vec<String> {
        self.tiles().into_iter().map(|t| t.block_id).collect()
    }

    fn first_block(&self) -> &str {
        match self {
            Layout::Block { block_id, .. } => block_id,
            Layout::Split { before, .. } => before.first_block(),
        }
    }
}

fn create_block(dir: &Path, startup: Option<&str>) -> Value {
    json!({"block": {"flavor": TERMINAL_FLAVOR, "options": terminal_options(dir, startup)}})
}

/// A config pane stack as a Rex create layout: vertical splits nested to the right, each
/// pane keeping its fraction of what remains. An empty stack is one bare shell.
pub fn stack_layout(panes: &[StackPane], dir: &Path) -> Value {
    fn node(panes: &[StackPane], remaining: f64, dir: &Path) -> Value {
        let p = &panes[0];
        let block = create_block(dir, p.startup.as_deref());
        if panes.len() == 1 {
            return block;
        }
        json!({"split": {
            "direction": "vertical",
            "ratio": p.size / remaining,
            "before": block,
            "after": node(&panes[1..], remaining - p.size, dir),
        }})
    }
    if panes.is_empty() {
        return create_block(dir, None);
    }
    node(panes, panes.iter().map(|p| p.size).sum(), dir)
}

/// The split ratio for a divider dragged to `pointer`, a window-fraction coordinate along
/// the divider's direction, relative to the split's own rect.
pub fn divider_ratio(d: &Divider, pointer: f64) -> f64 {
    let (origin, extent) = match d.dir {
        Dir::Horizontal => (d.rect.x, d.rect.w),
        Dir::Vertical => (d.rect.y, d.rect.h),
    };
    ((pointer - origin) / extent).clamp(0.05, 0.95)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn nested() -> Value {
        json!({
            "rect": [0.0, 0.0, 1.0, 1.0],
            "split": {
                "split_id": "s1", "direction": "vertical", "ratio": 0.5,
                "before": {"rect": [0.0, 0.0, 1.0, 0.5], "block_id": "a"},
                "after": {
                    "rect": [0.0, 0.5, 1.0, 0.5],
                    "split": {
                        "split_id": "s2", "direction": "horizontal", "ratio": 0.5,
                        "before": {"rect": [0.0, 0.5, 0.5, 0.5], "block_id": "b"},
                        "after": {"rect": [0.5, 0.5, 0.5, 0.5], "block_id": "c"}
                    }
                }
            }
        })
    }

    fn frac(x: f64, y: f64, w: f64, h: f64) -> Frac {
        Frac { x, y, w, h }
    }

    #[test]
    fn parses_single_block() {
        let l = Layout::parse(&json!({"rect": [0, 0, 1, 1], "block_id": "a"})).unwrap();
        assert_eq!(l, Layout::single("a"));
        assert_eq!(l.blocks(), ["a"]);
        assert!(l.dividers().is_empty());
    }

    #[test]
    fn parses_nested_split() {
        let l = Layout::parse(&nested()).unwrap();
        assert_eq!(
            l.tiles(),
            vec![
                Tile {
                    block_id: "a".into(),
                    rect: frac(0.0, 0.0, 1.0, 0.5)
                },
                Tile {
                    block_id: "b".into(),
                    rect: frac(0.0, 0.5, 0.5, 0.5)
                },
                Tile {
                    block_id: "c".into(),
                    rect: frac(0.5, 0.5, 0.5, 0.5)
                },
            ]
        );
        assert_eq!(l.blocks(), ["a", "b", "c"]);
    }

    #[test]
    fn unknown_direction_is_horizontal() {
        let mut n = nested();
        n["split"]["direction"] = json!("diagonal");
        let l = Layout::parse(&n).unwrap();
        assert_eq!(l.dividers()[0].dir, Dir::Horizontal);
    }

    #[test]
    fn null_layout_is_none() {
        assert_eq!(Layout::parse(&Value::Null), None);
        assert_eq!(Layout::parse(&json!({"rect": [0, 0, 1, 1]})), None);
    }

    #[test]
    fn dividers_name_first_leaf_each_side() {
        let d = Layout::parse(&nested()).unwrap().dividers();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].split_id, "s1");
        assert_eq!(
            (d[0].before_block.as_str(), d[0].after_block.as_str()),
            ("a", "b")
        );
        assert_eq!(
            (d[1].before_block.as_str(), d[1].after_block.as_str()),
            ("b", "c")
        );
    }

    #[test]
    fn divider_ratio_is_relative_to_its_split() {
        let d = Layout::parse(&nested()).unwrap().dividers();
        assert_eq!(divider_ratio(&d[1], 0.25), 0.25);
        let mut right = d[1].clone();
        right.rect = frac(0.5, 0.0, 0.5, 1.0);
        assert_eq!(divider_ratio(&right, 0.75), 0.5);
        // vertical, inner rect y = 0.5, h = 0.5
        let mut v = d[0].clone();
        v.rect = frac(0.0, 0.5, 1.0, 0.5);
        assert_eq!(divider_ratio(&v, 0.625), 0.25);
        assert_eq!(divider_ratio(&v, 0.0), 0.05);
        assert_eq!(divider_ratio(&v, 2.0), 0.95);
    }

    fn pane(size: f64, startup: Option<&str>) -> StackPane {
        StackPane {
            size,
            startup: startup.map(str::to_owned),
        }
    }

    fn blk(dir: &Path, startup: Option<&str>) -> Value {
        json!({"block": {"flavor": TERMINAL_FLAVOR, "options": terminal_options(dir, startup)}})
    }

    #[test]
    fn stack_layout_three_panes() {
        let dir = PathBuf::from("/work");
        let v = stack_layout(
            &[
                pane(0.2, Some("top")),
                pane(0.3, None),
                pane(0.5, Some("bot")),
            ],
            &dir,
        );
        let s1 = &v["split"];
        assert_eq!(s1["direction"], "vertical");
        assert!((s1["ratio"].as_f64().unwrap() - 0.2).abs() < 1e-9);
        assert_eq!(s1["before"], blk(&dir, Some("top")));
        let s2 = &s1["after"]["split"];
        assert_eq!(s2["direction"], "vertical");
        assert!((s2["ratio"].as_f64().unwrap() - 0.375).abs() < 1e-9);
        assert_eq!(s2["before"], blk(&dir, None));
        assert_eq!(s2["after"], blk(&dir, Some("bot")));
    }

    #[test]
    fn one_pane_is_a_bare_block() {
        let dir = PathBuf::from("/work");
        assert_eq!(
            stack_layout(&[pane(1.0, Some("x"))], &dir),
            blk(&dir, Some("x"))
        );
    }

    #[test]
    fn empty_stack_is_one_bare_shell() {
        let dir = PathBuf::from("/work");
        assert_eq!(stack_layout(&[], &dir), blk(&dir, None));
    }
}
