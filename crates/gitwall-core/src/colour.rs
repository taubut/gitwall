//! Sorting and filtering wallpapers by colour.
//!
//! Every thumbnail already yields a dominant accent (see `thumb.rs`), so
//! grouping by colour is just bucketing that hue. The one subtlety is that the
//! accent is deliberately clamped into a legible band for UI use — a nearly
//! greyscale wallpaper still gets handed a vivid fallback. Classifying on the
//! accent alone would therefore file every monochrome wallpaper under cyan,
//! which is why `ThumbMeta` carries a separate `mono` flag.

use serde::{Deserialize, Serialize};

/// Number of hue buckets around the wheel. Twelve reads well as a swatch strip
/// and keeps neighbouring hues distinguishable.
pub const HUES: u8 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Swatch {
    /// Greyscale, near-black or near-white — no usable hue.
    Mono,
    /// Bucket index, `0..HUES`, starting at red and going round.
    Hue(u8),
}

impl Swatch {
    /// Position for ordering: mono sorts last so colourful wallpapers lead.
    pub fn order_key(self) -> u16 {
        match self {
            Swatch::Hue(h) => h as u16,
            Swatch::Mono => u16::from(HUES) + 1,
        }
    }

    /// A representative colour, for drawing the filter strip.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Swatch::Mono => [0x8a, 0x90, 0x9e],
            Swatch::Hue(h) => {
                // `classify` centres bucket h on h*(360/HUES), so the
                // representative colour is that angle exactly — not the start
                // of a bucket running from it.
                let deg = h as f32 / HUES as f32 * 360.0;
                hsv_to_rgb(deg, 0.72, 0.92)
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Swatch::Mono => "mono",
            Swatch::Hue(h) => [
                "red", "orange", "yellow", "lime", "green", "spring", "cyan", "azure", "blue",
                "violet", "magenta", "rose",
            ][(h % HUES) as usize],
        }
    }

    /// Every swatch, in strip order.
    pub fn all() -> Vec<Swatch> {
        (0..HUES).map(Swatch::Hue).chain([Swatch::Mono]).collect()
    }
}

/// Bucket an image by its accent colour.
///
/// `mono` comes from the thumbnail pass and means "this image had no saturated
/// pixels worth sampling", which the accent value itself cannot tell you.
pub fn classify(accent: [u8; 3], mono: bool) -> Swatch {
    if mono {
        return Swatch::Mono;
    }
    let (h, s, v) = rgb_to_hsv(accent);
    if s < 0.12 || v < 0.10 {
        return Swatch::Mono;
    }
    // Offset by half a bucket so pure red (0°) lands in the middle of bucket 0
    // rather than straddling the wrap-around.
    let shifted = (h + 360.0 / HUES as f32 / 2.0) % 360.0;
    Swatch::Hue(((shifted / 360.0) * HUES as f32) as u8 % HUES)
}

fn rgb_to_hsv(c: [u8; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;

    let h = if d.abs() < f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };

    let h = if h < 0.0 { h + 360.0 } else { h };
    let s = if max <= f32::EPSILON { 0.0 } else { d / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        (((r + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
        (((g + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
        (((b + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_colours_land_in_the_bucket_you_would_name() {
        assert_eq!(classify([255, 0, 0], false).label(), "red");
        assert_eq!(classify([0, 255, 0], false).label(), "green");
        assert_eq!(classify([0, 0, 255], false).label(), "blue");
        assert_eq!(classify([0, 255, 255], false).label(), "cyan");
    }

    #[test]
    fn red_does_not_straddle_the_hue_wrap() {
        // Both sides of 0 degrees must classify the same, or reds split across
        // opposite ends of the strip.
        let just_under = classify([255, 0, 12], false);
        let just_over = classify([255, 12, 0], false);
        assert_eq!(just_under, just_over);
        assert_eq!(just_under, Swatch::Hue(0));
    }

    #[test]
    fn the_mono_flag_wins_over_a_vivid_fallback_accent() {
        // A greyscale wallpaper is handed the cyan fallback accent by the
        // thumbnail pass. It must still classify as mono.
        let fallback = [0x7f, 0xd4, 0xe8];
        assert_eq!(classify(fallback, true), Swatch::Mono);
        assert_eq!(classify(fallback, false), Swatch::Hue(6), "sanity: that is cyan");
    }

    #[test]
    fn desaturated_and_black_accents_are_mono() {
        assert_eq!(classify([128, 128, 128], false), Swatch::Mono);
        assert_eq!(classify([10, 10, 12], false), Swatch::Mono);
    }

    #[test]
    fn every_bucket_is_reachable_and_round_trips() {
        for h in 0..HUES {
            let s = Swatch::Hue(h);
            assert_eq!(classify(s.rgb(), false), s, "swatch {h} did not round-trip");
        }
    }

    #[test]
    fn mono_sorts_after_every_hue() {
        for h in 0..HUES {
            assert!(Swatch::Hue(h).order_key() < Swatch::Mono.order_key());
        }
    }

    #[test]
    fn all_lists_every_swatch_once() {
        let all = Swatch::all();
        assert_eq!(all.len(), HUES as usize + 1);
        assert_eq!(all.last(), Some(&Swatch::Mono));
    }
}
