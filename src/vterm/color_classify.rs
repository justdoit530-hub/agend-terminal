//! Cell foreground color classification — extracted from vterm.rs (anti-monolith split).
//! Single source of truth for the HIGH_FP color anchor's red predicate across all SGR encodings.
//! Moved out of vterm.rs to make room under its grandfathered LOC ceiling (see
//! tests/src_file_size_invariant.rs) ahead of the #3175 cursor-anchor port.

use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// Cached once: whether terminal supports true color (avoids env var lookup per cell).
pub(super) fn supports_truecolor() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let val = std::env::var("COLORTERM").unwrap_or_default();
        val.contains("truecolor") || val.contains("24bit")
    })
}

/// #1450: per-character foreground classification, aligned 1:1 with the
/// chars of the `String` returned by [`VTerm::tail_lines_with_fg`].
///
/// State detection's HIGH_FP color anchor (replaces the #919 raw-byte SGR
/// ring) reads this to decide whether a matched error phrase is rendered in
/// red. Because the classification comes off the *resolved* alacritty grid
/// cell, it is encoding-agnostic: alacritty's `Processor` has already parsed
/// 16-color (`\x1b[31m`), 256-color (`\x1b[38;5;Nm`) and 24-bit truecolor
/// (`\x1b[38;2;R;G;Bm`) into a normalized `Color` — so the #919 bugs (the
/// allow-list only knew 4 sixteen-color escapes, and raw-byte fragmentation
/// from Ink redraws) cannot recur here.
///
/// The non-red variants carry their source encoding purely for
/// observability — the suppress-path WARN log prints them so a future
/// incident can tell "real red mis-classified" (tune the predicate) apart
/// from "genuinely not red" (correct suppression).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellFg {
    /// Default foreground (terminal default, no SGR color).
    Default,
    /// Any red across the three encodings — the anchor signal.
    Red,
    /// A non-red named (16-color) foreground.
    Named,
    /// A non-red 256-color indexed foreground.
    Indexed(u8),
    /// A non-red 24-bit truecolor foreground.
    Rgb(u8, u8, u8),
}

impl CellFg {
    /// True iff this cell's foreground is classified red (the anchor signal).
    pub fn is_red(self) -> bool {
        matches!(self, CellFg::Red)
    }
}

/// #1450: is a 256-color palette index a "red"?
///
/// - 1 / 9 = the system red / bright-red slots.
/// - 16..=231 = the 6×6×6 RGB cube, index = 16 + 36·r + 6·g + b with each
///   channel in 0..=5. Red ⇔ the red channel dominates (`r ≥ 3`) and green /
///   blue stay low (`≤ 1`).
///
/// Grayscale ramp (232..=255) is never red.
pub(super) fn is_red_indexed(idx: u8) -> bool {
    if idx == 1 || idx == 9 {
        return true;
    }
    if (16..=231).contains(&idx) {
        let c = idx - 16;
        let r = c / 36;
        let g = (c % 36) / 6;
        let b = c % 6;
        return r >= 3 && g <= 1 && b <= 1;
    }
    false
}

/// #1450 / #1538: is a 24-bit RGB foreground a "red"?
///
/// HSV-based (not the old `r ≥ 2·g, 2·b` ratio). The ratio rejected pastel /
/// desaturated theme reds as false negatives — rgb(237,135,150) (hue≈351°) has
/// 237 < 2·135 so it failed — and loosening the ratio to catch them would
/// re-admit saturated orange (rgb(255,165,0): 255 ≥ 1.5·165), regressing #919.
/// Hue separates them cleanly: pastel red sits at ~351°, orange at ~39°.
///
/// Red iff hue ∈ [345°,360°) ∪ [0°,15°] AND saturation > 0.3 AND value > 0.4.
/// The sat/val floors drop greys and near-black/near-white prose foregrounds
/// (achromatic cells have no hue → not red). Covers chalk reds rgb(215,40,40) /
/// rgb(204,0,0) (hue 0°) and pastel reds rgb(237,135,150) (~351°) while
/// rejecting orange / amber / green / blue and unsaturated prose fg.
pub(super) fn is_red_rgb(r: u8, g: u8, b: u8) -> bool {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if delta == 0 {
        return false; // achromatic (grey/black/white) → no hue → not red
    }
    let sat = delta as f32 / max as f32; // max > 0 since delta > 0
    let value = max as f32 / 255.0;
    if sat <= 0.3 || value <= 0.4 {
        return false;
    }
    let (rf, gf, bf, d) = (r as f32, g as f32, b as f32, delta as f32);
    // Hue in degrees [0,360). `rem_euclid` keeps the red wrap-around positive.
    let hue = if max == r {
        60.0 * ((gf - bf) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((bf - rf) / d + 2.0)
    } else {
        60.0 * ((rf - gf) / d + 4.0)
    };
    hue <= 15.0 || hue >= 345.0
}

/// #1450: classify an alacritty cell foreground into a [`CellFg`], flagging
/// red across all three SGR encodings. Single source of truth for the
/// HIGH_FP color anchor's red predicate.
pub(super) fn classify_fg(color: Color) -> CellFg {
    match color {
        Color::Named(NamedColor::Red | NamedColor::BrightRed | NamedColor::DimRed) => CellFg::Red,
        Color::Named(NamedColor::Foreground | NamedColor::Background) => CellFg::Default,
        Color::Named(_) => CellFg::Named,
        Color::Indexed(idx) => {
            if is_red_indexed(idx) {
                CellFg::Red
            } else {
                CellFg::Indexed(idx)
            }
        }
        Color::Spec(rgb) => {
            if is_red_rgb(rgb.r, rgb.g, rgb.b) {
                CellFg::Red
            } else {
                CellFg::Rgb(rgb.r, rgb.g, rgb.b)
            }
        }
    }
}
