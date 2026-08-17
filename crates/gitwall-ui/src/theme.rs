//! Palette, fonts and slice metrics.
//!
//! Same design language as before: Fira Sans Compressed for the wallpaper name
//! (compressed like the slices), JetBrains Mono for everything the machine
//! knows — paths, shas, counts, dimensions.

use std::sync::Arc;

use egui::{Color32, FontData, FontDefinitions, FontFamily, Vec2};

pub const INK_900: Color32 = Color32::from_rgb(0x07, 0x08, 0x0b);
pub const INK_700: Color32 = Color32::from_rgb(0x11, 0x14, 0x1c);
pub const INK_600: Color32 = Color32::from_rgb(0x19, 0x1e, 0x28);

pub const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf4, 0xf9);
pub const DIM: Color32 = Color32::from_rgb(0xa2, 0xa9, 0xba);
pub const FAINT: Color32 = Color32::from_rgb(0x7b, 0x83, 0x98);

pub const ACCENT_FALLBACK: Color32 = Color32::from_rgb(0x7f, 0xd4, 0xe8);
pub const BAD: Color32 = Color32::from_rgb(0xff, 0xc9, 0xc9);

/// Slice ratios, all derived from slice height. Taken from skwd-wall's "M"
/// preset; see the notes in `Metrics`.
const R_SLICE_W: f32 = 0.18;
const R_EXPANDED: f32 = 768.0 / 432.0; // 16:9
const R_GAP: f32 = -0.055; // negative on purpose — slices overlap
const R_SKEW: f32 = 0.20;

const HEIGHT_RATIO: f32 = 0.40;
const EXPANDED_MAX_W: f32 = 0.40;

/// Font families, resolved once so a missing font file degrades to egui's
/// built-ins instead of panicking at draw time.
#[derive(Clone)]
pub struct Fonts {
    pub display: FontFamily,
    pub mono: FontFamily,
}

fn first_readable(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

pub fn install_fonts(ctx: &egui::Context) -> Fonts {
    let mut defs = FontDefinitions::default();

    // Neither JetBrains Mono nor Fira Sans Compressed carries the geometric
    // symbols the toolbar uses (★ ▤ ▦ ◐), so without a fallback they render as
    // tofu. DejaVu Sans has all of them and ships with practically every
    // distro. egui walks a family's font list until one has the glyph, so
    // appending it costs nothing for text that resolves earlier.
    let fallback = first_readable(&[
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ])
    .map(|bytes| {
        defs.font_data
            .insert("fallback".to_owned(), Arc::new(FontData::from_owned(bytes)));
        "fallback".to_owned()
    });

    let with_fallback = |primary: &str| {
        let mut v = vec![primary.to_owned()];
        v.extend(fallback.clone());
        v
    };

    let display = match first_readable(&[
        "/usr/share/fonts/TTF/FiraSansCompressed-Medium.ttf",
        "/usr/share/fonts/fira-sans/FiraSansCompressed-Medium.ttf",
        "/usr/share/fonts/TTF/FiraSansCondensed-Medium.ttf",
    ]) {
        Some(bytes) => {
            defs.font_data
                .insert("display".to_owned(), Arc::new(FontData::from_owned(bytes)));
            defs.families
                .insert(FontFamily::Name("display".into()), with_fallback("display"));
            FontFamily::Name("display".into())
        }
        None => FontFamily::Proportional,
    };

    let mono = match first_readable(&[
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf",
    ]) {
        Some(bytes) => {
            defs.font_data
                .insert("mono".to_owned(), Arc::new(FontData::from_owned(bytes)));
            defs.families
                .insert(FontFamily::Name("mono".into()), with_fallback("mono"));
            FontFamily::Name("mono".into())
        }
        None => FontFamily::Monospace,
    };

    ctx.set_fonts(defs);
    Fonts { display, mono }
}

/// Slice geometry for the current window size. Everything hangs off
/// `slice_h`, so one number rescales the whole strip.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub slice_h: f32,
    pub slice_w: f32,
    pub gap: f32,
    pub skew: f32,
    pub expanded: f32,
    pub radius: f32,
}

impl Metrics {
    pub fn new(size: Vec2) -> Self {
        let slice_h = (size.y * HEIGHT_RATIO).clamp(220.0, 700.0);
        Self {
            slice_h,
            slice_w: slice_h * R_SLICE_W,
            gap: slice_h * R_GAP,
            skew: slice_h * R_SKEW,
            expanded: (slice_h * R_EXPANDED).min(size.x * EXPANDED_MAX_W),
            radius: 12.0,
        }
    }

    /// Horizontal pitch between consecutive collapsed slices.
    pub fn step(&self) -> f32 {
        self.slice_w + self.gap
    }
}

/// Premultiplied tint that both dims and fades a texture in one vertex colour.
pub fn tint(brightness: f32, alpha: f32) -> Color32 {
    let a = alpha.clamp(0.0, 1.0);
    let b = brightness.clamp(0.0, 1.0);
    let to_u8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_premultiplied(to_u8(b * a), to_u8(b * a), to_u8(b * a), to_u8(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_overlap_rather_than_sitting_apart() {
        let m = Metrics::new(Vec2::new(2560.0, 1440.0));
        assert!(m.gap < 0.0, "gap must be negative");
        assert!(
            m.step() < m.slice_w,
            "pitch {} should be less than slice width {} so they overlap",
            m.step(),
            m.slice_w
        );
    }

    #[test]
    fn expanded_slice_never_eats_the_whole_screen() {
        for w in [1280.0, 1600.0, 2560.0, 3440.0] {
            let m = Metrics::new(Vec2::new(w, 1440.0));
            assert!(
                m.expanded <= w * EXPANDED_MAX_W + 0.01,
                "expanded {} too wide for a {w}px window",
                m.expanded
            );
        }
    }

    #[test]
    fn slice_height_is_clamped_on_extreme_windows() {
        assert!(Metrics::new(Vec2::new(800.0, 200.0)).slice_h >= 220.0);
        assert!(Metrics::new(Vec2::new(3840.0, 4000.0)).slice_h <= 700.0);
    }

    #[test]
    fn tint_is_premultiplied_so_alpha_never_brightens() {
        let t = tint(1.0, 0.5);
        assert_eq!(t.a(), 128);
        assert!(t.r() <= t.a(), "premultiplied channels cannot exceed alpha");

        let dark = tint(0.4, 1.0);
        assert_eq!(dark.a(), 255);
        assert_eq!(dark.r(), 102);

        let gone = tint(1.0, 0.0);
        assert_eq!(gone.a(), 0);
        assert_eq!(gone.r(), 0);
    }
}
