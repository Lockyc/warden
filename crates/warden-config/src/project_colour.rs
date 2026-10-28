//! Per-project colour: a pinned `project_colour` wins, everything else gets a stable pick from
//! [`PROJECT_POOL`], so a project is recognisable at a glance across runs and config edits.

use crate::{Colour, Tab};
use std::collections::BTreeMap;

/// Twelve distinct hues, each at least 4.5:1 against white text (the contrast test pins it).
pub const PROJECT_POOL: [&str; 12] = [
    "#c62828", "#bf360c", "#9a5b00", "#6b7a00", "#2e7d32", "#00796b", "#00838f", "#1565c0",
    "#3949ab", "#6a1b9a", "#ad1457", "#5d4037",
];

/// FNV-1a, 64-bit. Hand-rolled because std's `DefaultHasher` makes no stability promise across
/// Rust versions or runs, and an assigned colour must never change under a project.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn pool() -> impl Iterator<Item = Colour> {
    PROJECT_POOL
        .iter()
        .map(|s| Colour::parse(s).expect("PROJECT_POOL entries are valid hex"))
}

/// The colour of every tab, keyed by `Tab::key`: a pinned tab keeps its pin; the rest pick
/// `fnv1a_64(key) % n` from the pool minus every pinned value (the full pool when that leaves
/// nothing). Independent of tab order, so reordering or adding tabs never recolours a project
/// — except that pinning a pool colour removes it from the others' pool.
pub fn assign_project_colours(tabs: &[Tab]) -> BTreeMap<String, Colour> {
    let pinned: Vec<Colour> = tabs.iter().filter_map(|t| t.project_colour).collect();
    let mut available: Vec<Colour> = pool().filter(|c| !pinned.contains(c)).collect();
    if available.is_empty() {
        available = pool().collect();
    }
    tabs.iter()
        .map(|t| {
            let colour = t
                .project_colour
                .unwrap_or_else(|| available[(fnv1a_64(&t.key) % available.len() as u64) as usize]);
            (t.key.clone(), colour)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::parse;
    use crate::resolve::resolve;

    fn tabs(s: &str) -> Vec<Tab> {
        resolve(parse(s).unwrap()).unwrap().0.windows[0]
            .tabs
            .clone()
    }

    fn lin(c: u8) -> f64 {
        let v = f64::from(c) / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn pool_is_distinct_and_readable_with_white_text() {
        let cs: Vec<Colour> = pool().collect();
        for (i, a) in cs.iter().enumerate() {
            assert!(!cs[..i].contains(a), "duplicate pool colour {a:?}");
            let l = 0.2126 * lin(a.r) + 0.7152 * lin(a.g) + 0.0722 * lin(a.b);
            assert!(1.05 / (l + 0.05) >= 4.5, "{a:?} fails contrast");
        }
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a_64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn assignment_is_stable_across_calls_and_tab_order() {
        let mut ts = tabs(
            r##"
[[window]]
title = "w"
  [[window.tab]]
  dir = "/tmp/a"
  [[window.tab]]
  dir = "/tmp/b"
  [[window.tab]]
  dir = "/tmp/c"
"##,
        );
        let first = assign_project_colours(&ts);
        assert_eq!(first, assign_project_colours(&ts));
        ts.reverse();
        assert_eq!(first, assign_project_colours(&ts));
        assert_eq!(first.len(), 3);
    }

    #[test]
    fn pinned_pool_colour_is_excluded_from_others() {
        let ts = tabs(&format!(
            r##"
[[window]]
title = "w"
  [[window.tab]]
  dir = "/tmp/pinned"
  project_colour = "{}"
{}"##,
            PROJECT_POOL[0].to_uppercase(),
            (0..40)
                .map(|i| format!("  [[window.tab]]\n  dir = \"/tmp/p{i}\"\n"))
                .collect::<String>()
        ));
        let got = assign_project_colours(&ts);
        let pin = Colour::parse(PROJECT_POOL[0]).unwrap();
        assert_eq!(got[&ts[0].key], pin);
        assert!(got.iter().filter(|(_, c)| **c == pin).count() == 1);
    }

    #[test]
    fn short_hex_pin_matches_its_pool_entry() {
        // "#ccc" is not a pool colour; "#c62828" in long form is. A pin of either form removes it.
        let ts = tabs(
            r##"
[[window]]
title = "w"
  [[window.tab]]
  dir = "/tmp/x"
  project_colour = "#C62828"
"##,
        );
        assert_eq!(
            assign_project_colours(&ts)[&ts[0].key],
            Colour::parse("#c62828").unwrap()
        );
    }

    #[test]
    fn all_pool_pinned_falls_back_to_full_pool() {
        let mut toml = String::from("[[window]]\ntitle = \"w\"\n");
        for (i, c) in PROJECT_POOL.iter().enumerate() {
            toml +=
                &format!("  [[window.tab]]\n  dir = \"/tmp/pin{i}\"\n  project_colour = \"{c}\"\n");
        }
        toml += "  [[window.tab]]\n  dir = \"/tmp/free\"\n";
        let ts = tabs(&toml);
        let got = assign_project_colours(&ts);
        let free = got[&ts[12].key];
        assert!(pool().any(|c| c == free));
    }
}
