//! A project's colours for the agent pane's summary block: the title row's bar, the summary
//! rows' shade, and one text colour for both.
//!
//! Derived in OKLCH so lightness is perceptual: every generated bar sits at [`BAR_L`] and every
//! shade at [`SHADE_L`], whatever the hue, so no project's text is harder to read than another's.
//! The hue comes from a hash of the tab title, so a project keeps its colour across launches and
//! any hue is reachable. A tab's `colour` key pins the bar instead; the shade and text colour
//! still derive from it.

use warden_config::Colour;

/// OKLab lightness of a generated bar.
const BAR_L: f64 = 0.55;
/// OKLab lightness of the shade under white text.
const SHADE_L: f64 = 0.30;
/// OKLab lightness of the shade under dark text — a light pinned bar keeps a light shade, or
/// its dark text would vanish on the summary rows.
const SHADE_L_LIGHT: f64 = 0.88;
/// OKLCH chroma of a generated bar, and the cap on a shade's.
const CHROMA: f64 = 0.12;

const WHITE: [u8; 3] = [0xff, 0xff, 0xff];
const DARK: [u8; 3] = [0x11, 0x11, 0x11];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectColour {
    pub bar_bg: String,
    pub fg: String,
    pub shade_bg: String,
}

pub fn project_colour(title: &str, pin: Option<Colour>) -> ProjectColour {
    let bar = match pin {
        Some(c) => [c.r, c.g, c.b],
        None => oklch_in_gamut(BAR_L, CHROMA, hue(title)),
    };
    let fg = if contrast(WHITE, bar) >= contrast(DARK, bar) {
        WHITE
    } else {
        DARK
    };
    let (_, c, h) = srgb_to_oklch(bar);
    let shade_l = if fg == WHITE { SHADE_L } else { SHADE_L_LIGHT };
    let shade = oklch_in_gamut(shade_l, c.min(CHROMA), h);
    ProjectColour {
        bar_bg: hex(bar),
        fg: hex(fg),
        shade_bg: hex(shade),
    }
}

/// The title's hue in degrees: FNV-1a over its bytes. Stable across runs and platforms (not
/// `std`'s randomly seeded hasher).
fn hue(title: &str) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in title.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % 360) as f64
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The OKLCH colour as sRGB, chroma reduced until it fits the gamut (hue and lightness kept).
fn oklch_in_gamut(l: f64, c: f64, h: f64) -> [u8; 3] {
    let mut c = c;
    loop {
        if let Some(rgb) = oklch_to_srgb(l, c, h) {
            return rgb;
        }
        c = (c - 0.005).max(0.0);
    }
}

/// `None` when the colour lies outside sRGB. Chroma 0 is always inside for `l` in 0..=1.
fn oklch_to_srgb(l: f64, c: f64, h: f64) -> Option<[u8; 3]> {
    let (a, b) = (c * h.to_radians().cos(), c * h.to_radians().sin());
    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548 * b;
    let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
    let lin = [
        4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3,
        -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701 * s3,
    ];
    const EPS: f64 = 1e-6;
    if lin.iter().any(|&v| !(-EPS..=1.0 + EPS).contains(&v)) {
        return None;
    }
    Some(lin.map(|v| (encode(v.clamp(0.0, 1.0)) * 255.0).round() as u8))
}

/// sRGB → (L, C, h°).
fn srgb_to_oklch(rgb: [u8; 3]) -> (f64, f64, f64) {
    let [r, g, b] = rgb.map(|v| decode(f64::from(v) / 255.0));
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    let ll = 0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s;
    let a = 1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s;
    let bb = 0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s;
    (ll, a.hypot(bb), bb.atan2(a).to_degrees().rem_euclid(360.0))
}

fn decode(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn encode(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// WCAG 2 contrast ratio.
fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let lum = |c: [u8; 3]| {
        let [r, g, b] = c.map(|v| decode(f64::from(v) / 255.0));
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 8] = [
        "warden",
        "curator",
        "lector",
        "flotilla",
        "agentmux",
        "config-core",
        "chrome-core",
        "shell-core",
    ];

    fn rgb(hex: &str) -> [u8; 3] {
        let c = Colour::parse(hex).unwrap();
        [c.r, c.g, c.b]
    }

    fn hue_gap(a: f64, b: f64) -> f64 {
        let d = (a - b).rem_euclid(360.0);
        d.min(360.0 - d)
    }

    #[test]
    fn same_title_same_colour() {
        assert_eq!(
            project_colour("warden", None),
            project_colour("warden", None)
        );
    }

    #[test]
    fn constellation_hues_spread() {
        for (i, a) in NAMES.iter().enumerate() {
            for b in &NAMES[i + 1..] {
                let gap = hue_gap(hue(a), hue(b));
                assert!(gap >= 10.0, "{a} vs {b}: {gap:.1}°");
            }
        }
    }

    #[test]
    fn generated_bars_are_equally_light() {
        for n in NAMES {
            let c = project_colour(n, None);
            let (bl, _, _) = srgb_to_oklch(rgb(&c.bar_bg));
            let (sl, _, _) = srgb_to_oklch(rgb(&c.shade_bg));
            assert!((bl - BAR_L).abs() <= 0.01, "{n} bar L {bl}");
            assert!((sl - SHADE_L).abs() <= 0.01, "{n} shade L {sl}");
        }
    }

    #[test]
    fn pin_sets_bar_exactly() {
        let c = project_colour(
            "x",
            Some(Colour {
                r: 0x0f,
                g: 0x8a,
                b: 0x8a,
            }),
        );
        assert_eq!(c.bar_bg, "#0f8a8a");
        let (_, _, bh) = srgb_to_oklch(rgb(&c.bar_bg));
        let (_, _, sh) = srgb_to_oklch(rgb(&c.shade_bg));
        assert!(hue_gap(bh, sh) <= 5.0, "bar {bh} shade {sh}");
    }

    #[test]
    fn light_pin_gets_dark_text() {
        let light = project_colour(
            "x",
            Some(Colour {
                r: 0xff,
                g: 0xff,
                b: 0xaa,
            }),
        );
        assert_eq!(light.fg, "#111111");
        assert_ne!(light.shade_bg, light.bar_bg);
        let dark = project_colour("x", Some(Colour { r: 5, g: 5, b: 5 }));
        assert_eq!(dark.fg, "#ffffff");
        assert_ne!(dark.shade_bg, dark.bar_bg);
    }

    #[test]
    fn text_is_legible() {
        let pins = [
            Some(Colour {
                r: 0xff,
                g: 0xff,
                b: 0xaa,
            }),
            Some(Colour { r: 5, g: 5, b: 5 }),
        ];
        let cases = NAMES
            .iter()
            .map(|n| project_colour(n, None))
            .chain(pins.into_iter().map(|p| project_colour("x", p)));
        for c in cases {
            let fg = rgb(&c.fg);
            assert!(contrast(fg, rgb(&c.bar_bg)) >= 4.5, "{c:?} bar");
            assert!(contrast(fg, rgb(&c.shade_bg)) >= 4.5, "{c:?} shade");
        }
    }
}
