//! Decode, downscale, and pull an accent colour out of each image.
//!
//! CPU-bound and blocking; callers run this on a blocking thread (see
//! `fetch::Fetcher`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;
use crate::{Error, Result};

/// Long-edge target. The focused panel renders large on a 4K display, so 640
/// keeps it crisp while turning a ~1.8 MB source into roughly 40-60 KB.
pub const THUMB_MAX: u32 = 640;

const JPEG_QUALITY: u8 = 82;

/// Background used when flattening images that carry alpha. Matches the UI
/// backdrop so transparent PNGs don't come out with black fringes.
const FLATTEN_BG: [u8; 3] = [0x0B, 0x0D, 0x12];

/// Used when an image has no colour worth borrowing (pure greyscale, or so
/// dark that every pixel is filtered out).
const FALLBACK_ACCENT: [u8; 3] = [0x7F, 0xD4, 0xE8];

/// What the UI needs to know about an image beyond its pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ThumbMeta {
    /// Dimensions of the *original*, not the thumbnail — this is what gets
    /// shown to the user and what tells them if it fits their display.
    pub src_w: u32,
    pub src_h: u32,
    /// Dominant saturated hue, clamped to stay legible as UI chrome.
    pub accent: [u8; 3],
    /// True when the image had no saturated pixels worth sampling and `accent`
    /// is the fallback. Colour filtering needs this: the clamped accent alone
    /// cannot distinguish "actually cyan" from "greyscale, given cyan".
    ///
    /// `serde(default)` so sidecars written before this field stay readable.
    #[serde(default)]
    pub mono: bool,
}

/// Decode `bytes`, downscale to fit `max` on the long edge, write JPEG to `out`.
pub fn write_thumbnail(bytes: &[u8], out: &Path, max: u32, label: &str) -> Result<ThumbMeta> {
    let img = image::load_from_memory(bytes).map_err(|e| Error::Decode {
        path: label.to_string(),
        source: e,
    })?;

    let (src_w, src_h) = (img.width(), img.height());

    // `thumbnail` is a box filter: each source pixel lands in exactly one
    // target pixel. For the large reductions we're doing (4K -> 640) that's
    // both faster and less aliased than a resampling filter.
    let small = if src_w.max(src_h) > max {
        img.thumbnail(max, max)
    } else {
        img
    };

    // JPEG has no alpha channel, so flatten rather than letting `to_rgb8`
    // silently reinterpret transparent pixels as opaque.
    let rgb = match small.color() {
        image::ColorType::Rgba8
        | image::ColorType::Rgba16
        | image::ColorType::La8
        | image::ColorType::La16 => {
            let rgba = small.to_rgba8();
            let mut flat = image::RgbImage::new(rgba.width(), rgba.height());
            for (x, y, px) in rgba.enumerate_pixels() {
                let a = px[3] as u32;
                let blend =
                    |c: u8, bg: u8| -> u8 { ((c as u32 * a + bg as u32 * (255 - a)) / 255) as u8 };
                flat.put_pixel(
                    x,
                    y,
                    image::Rgb([
                        blend(px[0], FLATTEN_BG[0]),
                        blend(px[1], FLATTEN_BG[1]),
                        blend(px[2], FLATTEN_BG[2]),
                    ]),
                );
            }
            flat
        }
        _ => small.to_rgb8(),
    };

    // Sample a tiny copy — 48px is plenty to find a dominant hue and keeps
    // this well under a millisecond.
    let (accent, mono) = dominant_accent(
        &image::DynamicImage::ImageRgb8(rgb.clone())
            .thumbnail(48, 48)
            .to_rgb8(),
    );

    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| Error::Decode {
            path: label.to_string(),
            source: e,
        })?;

    write_atomic(out, &buf)?;

    Ok(ThumbMeta {
        src_w,
        src_h,
        accent,
        mono,
    })
}

/// Pick the hue the image is "about".
///
/// Straight averaging turns every photo into the same brown-grey, so instead we
/// bucket by hue, weight each pixel by saturation x value, and take the heaviest
/// bucket. The result is then clamped into a band that stays readable against a
/// near-black UI — an accent borrowed from a dark image still has to work as an
/// outline colour.
/// Returns the accent and whether it is the fallback (no usable hue found).
fn dominant_accent(img: &image::RgbImage) -> ([u8; 3], bool) {
    const BUCKETS: usize = 24;
    let mut weight = [0f64; BUCKETS];
    let mut sat = [0f64; BUCKETS];
    let mut val = [0f64; BUCKETS];

    for px in img.pixels() {
        let (h, s, v) = rgb_to_hsv(px[0], px[1], px[2]);
        // Near-black and near-grey pixels carry no usable hue.
        if v < 0.15 || s < 0.18 {
            continue;
        }
        let b = ((h / 360.0) * BUCKETS as f64) as usize;
        let b = b.min(BUCKETS - 1);
        let w = s * v;
        weight[b] += w;
        sat[b] += s * w;
        val[b] += v * w;
    }

    let best = weight
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);

    if weight[best] <= f64::EPSILON {
        return (FALLBACK_ACCENT, true);
    }

    let h = (best as f64 + 0.5) / BUCKETS as f64 * 360.0;
    let s = (sat[best] / weight[best]).clamp(0.55, 0.92);
    let v = (val[best] / weight[best]).clamp(0.70, 0.98);
    (hsv_to_rgb(h, s, v), false)
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;

    let h = if d.abs() < f64::EPSILON {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };

    let h = if h < 0.0 { h + 360.0 } else { h };
    let s = if max <= f64::EPSILON { 0.0 } else { d / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
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

    fn solid_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for px in img.pixels_mut() {
            *px = image::Rgba(rgba);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("gitwall-thumb-{}-{name}.jpg", std::process::id()))
    }

    #[test]
    fn downscales_large_images_and_reports_source_size() {
        let out = tmp("big");
        let m = write_thumbnail(&solid_png(2000, 1000, [200, 30, 40, 255]), &out, 640, "big").unwrap();
        assert_eq!((m.src_w, m.src_h), (2000, 1000), "reports the ORIGINAL size");

        let thumb = image::open(&out).unwrap();
        assert_eq!(thumb.width(), 640, "long edge hits the cap");
        assert_eq!(thumb.height(), 320, "aspect ratio preserved");
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn does_not_upscale_small_images() {
        let out = tmp("small");
        write_thumbnail(&solid_png(100, 50, [200, 30, 40, 255]), &out, 640, "small").unwrap();
        let thumb = image::open(&out).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (100, 50));
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn transparent_pixels_flatten_to_the_ui_background_not_black() {
        let out = tmp("alpha");
        write_thumbnail(&solid_png(64, 64, [200, 30, 40, 0]), &out, 640, "alpha").unwrap();
        let px = image::open(&out).unwrap().to_rgb8();
        let got = px.get_pixel(32, 32).0;
        for i in 0..3 {
            assert!(
                got[i].abs_diff(FLATTEN_BG[i]) < 12,
                "channel {i}: got {got:?}, want ~{FLATTEN_BG:?}"
            );
        }
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn corrupt_bytes_error_instead_of_panicking() {
        let out = tmp("junk");
        let err = write_thumbnail(b"definitely not an image", &out, 640, "junk.png");
        assert!(matches!(err, Err(Error::Decode { .. })));
        assert!(!out.exists(), "must not leave a partial file behind");
    }

    #[test]
    fn accent_follows_the_dominant_hue() {
        let red = image::RgbImage::from_pixel(16, 16, image::Rgb([190, 40, 40]));
        let (h, _, _) = {
            let (a, mono) = dominant_accent(&red);
            assert!(!mono, "a red image is not monochrome");
            rgb_to_hsv(a[0], a[1], a[2])
        };
        assert!(h < 25.0 || h > 335.0, "red image should yield a red-ish accent, got hue {h}");

        let teal = image::RgbImage::from_pixel(16, 16, image::Rgb([30, 150, 160]));
        let (a, _) = dominant_accent(&teal);
        let (h, _, _) = rgb_to_hsv(a[0], a[1], a[2]);
        assert!((160.0..210.0).contains(&h), "teal image should stay teal, got hue {h}");
    }

    #[test]
    fn greyscale_images_fall_back_instead_of_producing_mud() {
        // The `mono` flag is what colour filtering keys on — the accent itself
        // is a vivid fallback and would otherwise be filed under cyan.
        let grey = image::RgbImage::from_pixel(16, 16, image::Rgb([90, 90, 90]));
        assert_eq!(dominant_accent(&grey), (FALLBACK_ACCENT, true));

        let black = image::RgbImage::from_pixel(16, 16, image::Rgb([2, 2, 3]));
        assert_eq!(dominant_accent(&black), (FALLBACK_ACCENT, true));
    }

    #[test]
    fn accent_from_a_dark_image_is_still_bright_enough_to_read_as_chrome() {
        // A moody wallpaper shouldn't hand back a colour invisible on near-black.
        let dark_blue = image::RgbImage::from_pixel(16, 16, image::Rgb([12, 20, 55]));
        let (a, _) = dominant_accent(&dark_blue);
        let (_, s, v) = rgb_to_hsv(a[0], a[1], a[2]);
        assert!(v >= 0.69, "value should be lifted into the legible band, got {v}");
        assert!(s >= 0.54, "saturation should be lifted too, got {s}");
    }
}
