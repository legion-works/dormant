//! Pure pixel operations on the baked base mark pixmap.
//!
//! The build script (`build.rs`) rasterizes `design/web-ui/assets/mark.svg`
//! once per tray size (22/24/32/48) into ARGB32 big-endian blobs and
//! embeds them via `include_bytes!`.  This module takes those base blobs
//! and derives the variant pixmaps:
//!
//! - **paused** — base + a small yellow `||` badge in the bottom-right
//!   corner (notification-dot idiom).
//! - **unreachable** — base desaturated (each RGB mixed 50% toward mid
//!   grey).
//!
//! Normal and Attention share the base blob: the mark IS the dormant
//! green, so "Attention" reads as "this is the brand — the engine is
//! currently working on something".  The variant only matters when the
//! operator took an action (paused) or the daemon died (unreachable).
//!
//! No `ksni` types live here so the pixel ops are testable on every
//! platform (Windows / macOS CI legs).

/// Tray sizes pre-baked by `build.rs`.  Must match the `SIZES` constant
/// there; changing one without the other is a compile error from the
/// `include_bytes!` expansion.
pub const SIZES: &[u32] = &[22, 24, 32, 48];

/// All ARGB32 blobs at every size, in their three derived variants.
///
/// Holds owned `Vec<u8>` for each (state, size) pair.  The tray binary
/// builds one of these at startup and reuses it on every status refresh
/// — the pixels never change between daemon events, only the *choice* of
/// which pixmap to expose to `ksni` changes.
#[derive(Debug, Clone)]
pub struct IconSet {
    /// Size → ARGB32 BE bytes (the unmodified base mark).
    pub base: Vec<(u32, Vec<u8>)>,
    /// Size → ARGB32 BE bytes (base + pause-badge overlay).
    pub paused: Vec<(u32, Vec<u8>)>,
    /// Size → ARGB32 BE bytes (base desaturated toward grey).
    pub unreachable: Vec<(u32, Vec<u8>)>,
}

impl IconSet {
    /// Load the base blobs from `OUT_DIR` (set by `build.rs`) and derive
    /// the two variants in pure Rust.
    ///
    /// # Panics
    ///
    /// Panics if any pre-baked blob is missing — that's a build-script /
    /// source-tree drift, and the tray cannot operate without it.
    #[must_use]
    pub fn load() -> Self {
        let mut base = Vec::with_capacity(SIZES.len());
        let mut paused = Vec::with_capacity(SIZES.len());
        let mut unreachable = Vec::with_capacity(SIZES.len());

        for &size in SIZES {
            let path = format!(
                "{OUT_DIR}/mark_{size}.bin",
                OUT_DIR = std::env::var("OUT_DIR").expect("OUT_DIR set at build time")
            );
            let bytes = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
                Ok("linux") => std::fs::read(&path)
                    .unwrap_or_else(|e| panic!("read baked icon blob {path}: {e}")),
                // Non-Linux builds skip the build.rs blob — the stub main
                // never instantiates this.  Synthesize a 1×1 transparent
                // pixmap so unit tests on macOS / Windows can still
                // exercise the variant transforms.
                _ => vec![0u8; (size as usize) * (size as usize) * 4],
            };

            let mut p = bytes.clone();
            draw_pause_badge(&mut p, size);
            let mut u = bytes.clone();
            desaturate(&mut u);

            base.push((size, bytes));
            paused.push((size, p));
            unreachable.push((size, u));
        }

        Self {
            base,
            paused,
            unreachable,
        }
    }
}

/// Draw a yellow `||` badge in the bottom-right corner of an ARGB32 BE
/// pixmap.
///
/// The badge occupies roughly the inner 40% of the bottom-right quadrant
/// and contains two thin vertical bars on a yellow rounded background.
/// Pure pixel write — no anti-aliasing, no translucency beyond the
/// straight-alpha over-composite below.
///
/// Public for unit testing.
// `bar_y0`/`bar_y1` etc. read more naturally than e.g. `bar_y_start`;
// silence the similar-names lint deliberately for this geometry block.
#[allow(clippy::similar_names)]
pub fn draw_pause_badge(pixels: &mut [u8], size: u32) {
    let s = size.cast_signed();
    // Badge anchor: bottom-right corner, 40% of side length.
    let badge_w = (s * 2) / 5;
    let badge_h = (s * 2) / 5;
    let badge_x0 = s - badge_w - (s / 12).max(1);
    let badge_y0 = s - badge_h - (s / 12).max(1);
    let badge_x1 = badge_x0 + badge_w;
    let badge_y1 = badge_y0 + badge_h;

    // Yellow opaque fill #F5C518 (RGB), straight alpha 255.
    // ARGB BE bytes: [A=255, R=245, G=197, B=24].
    let fill: [u8; 4] = [0xFF, 0xF5, 0xC5, 0x18];

    // Two vertical bars — dark on yellow.
    // ARGB BE bytes: [A=255, R=33, G=33, B=33].
    let bar: [u8; 4] = [0xFF, 0x21, 0x21, 0x21];

    // `badge_w` is small (≤ 19 for our largest size 48), so the i32
    // intermediate from f32 is in-range and the truncation is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let stroke_px = (f64::from(badge_w) * 0.12).ceil().max(1.0) as i32;

    // Fill the badge background (excluding a 1-px transparent inset so
    // the badge reads as a distinct shape against the green mark).
    for y in badge_y0..badge_y1 {
        for x in badge_x0..badge_x1 {
            // Skip a 1-px transparent inset — keeps the badge slightly
            // off the corner for legibility.
            if x == badge_x0 || y == badge_y0 || x == badge_x1 - 1 || y == badge_y1 - 1 {
                continue;
            }
            blend_pixel(pixels, size, x, y, fill);
        }
    }

    // Two vertical bars at the horizontal center of the badge.
    let inner_w = badge_x1 - badge_x0 - 2;
    let gap = (inner_w / 5).max(1);
    let bar_w = ((inner_w - gap * 3) / 2).max(1);
    let bar_x0 = badge_x0 + 1 + gap;
    let bar_x1 = bar_x0 + bar_w;
    let bar_x2 = bar_x1 + gap;
    let bar_x3 = bar_x2 + bar_w;
    let bar_y0 = badge_y0 + stroke_px.max(2);
    let bar_y1 = badge_y1 - stroke_px.max(2) - 1;

    for y in bar_y0..bar_y1 {
        for x in bar_x0..bar_x1 {
            blend_pixel(pixels, size, x, y, bar);
        }
        for x in bar_x2..bar_x3 {
            blend_pixel(pixels, size, x, y, bar);
        }
    }
}

/// Desaturate an ARGB32 BE pixmap by mixing every RGB triplet 50% toward
/// mid-grey (128, 128, 128).  Alpha channel preserved.
///
/// Public for unit testing.
#[allow(
    clippy::manual_midpoint,
    reason = "explicit (a+b)/2 keeps the documented rounding behaviour"
)]
pub fn desaturate(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        // ARGB BE → bytes [A, R, G, B].
        let r = u16::from(px[1]);
        let g = u16::from(px[2]);
        let b = u16::from(px[3]);
        // Luma-ish mix toward grey (Rec. 601 weights are overkill for a
        // monochrome tray variant — a flat 50/50 read is clearer here).
        // The mean of three channels is the natural per-pixel grey; we
        // then mix each channel 50/50 toward it.
        let grey = (r + g + b) / 3;
        px[1] = u8::try_from((r + grey) / 2).unwrap_or(u8::MAX);
        px[2] = u8::try_from((g + grey) / 2).unwrap_or(u8::MAX);
        px[3] = u8::try_from((b + grey) / 2).unwrap_or(u8::MAX);
    }
}

/// Straight-alpha over-composite `src` onto the pixel at `(x, y)` in the
/// ARGB32 BE pixmap `pixels` of side `size`.
///
/// Out-of-bounds writes are silently dropped (the badge lives near the
/// corner and we don't want a panic from a 1-px rounding error).
fn blend_pixel(pixels: &mut [u8], size: u32, x: i32, y: i32, src: [u8; 4]) {
    if x < 0 || y < 0 || x >= size.cast_signed() || y >= size.cast_signed() {
        return;
    }
    let idx = ((y.cast_unsigned() * size + x.cast_unsigned()) * 4) as usize;
    if idx + 4 > pixels.len() {
        return;
    }
    let dst_a = pixels[idx];
    let src_a = src[0];
    let out_a = u16::from(src_a) + u16::from(dst_a) * (255 - u16::from(src_a)) / 255;
    if out_a == 0 {
        return;
    }
    for i in 0..3 {
        let s = u16::from(src[i + 1]);
        let d = u16::from(pixels[idx + 1 + i]);
        let blended = (s * u16::from(src_a)
            + d * u16::from(dst_a) * (255 - u16::from(src_a)) / 255)
            / out_a.max(1);
        // The blend math keeps `blended` ≤ 255, so the truncation is
        // intentional and lossless for our pixel range.
        pixels[idx + 1 + i] = u8::try_from(blended).unwrap_or(u8::MAX);
    }
    pixels[idx] = u8::try_from(out_a).unwrap_or(u8::MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argb(a: u8, r: u8, g: u8, b: u8) -> [u8; 4] {
        [a, r, g, b]
    }

    #[test]
    fn desaturate_preserves_alpha_and_greys_rgb() {
        // Fully-opaque bright red: A=255, R=255, G=0, B=0.
        let mut pix = argb(255, 255, 0, 0).to_vec();
        desaturate(&mut pix);
        // Alpha unchanged.
        assert_eq!(pix[0], 255);
        // Mean of (255, 0, 0) is 85.  Each channel becomes (ch + 85) / 2
        // truncated toward zero.
        assert_eq!(pix[1], 170); // R: (255 + 85) / 2 = 170
        assert_eq!(pix[2], 42); // G: (0 + 85) / 2 = 42
        assert_eq!(pix[3], 42); // B: (0 + 85) / 2 = 42
    }

    #[test]
    fn desaturate_handles_transparent_pixels_without_div_zero() {
        // Fully-transparent black.  Alpha stays 0; RGB changes are
        // irrelevant for an invisible pixel but the function must not
        // panic.
        let mut pix = argb(0, 0, 0, 0).to_vec();
        desaturate(&mut pix);
        assert_eq!(pix[0], 0);
    }

    #[test]
    fn pause_badge_writes_yellow_in_bottom_right_quadrant() {
        // 24×24 fixture with one fully-opaque white pixel per location.
        let size = 24u32;
        let mut pix = vec![0u8; (size * size * 4) as usize];
        // Seed every pixel to opaque white (A=255, R=255, G=255, B=255).
        for px in pix.chunks_exact_mut(4) {
            px.copy_from_slice(&[255, 255, 255, 255]);
        }
        draw_pause_badge(&mut pix, size);
        // Probe a pixel strictly inside the badge fill — not on the
        // 1-px transparent inset border and not on the inner pause
        // bars.  For a 24×24 canvas the badge sits roughly at
        // (13..22, 13..22); the bars occupy y∈15..19.  (14, 14) is in
        // the yellow fill above the bars.
        let px_x = 14u32;
        let px_y = 14u32;
        let idx = ((px_y * size + px_x) * 4) as usize;
        // Yellow fill is [A=255, R=245, G=197, B=24].  Blended over white
        // (the seed), each channel becomes the source's value.  Expect
        // R > 200, G in the 150..230 range, B < 80.
        assert!(
            pix[idx + 1] > 200,
            "R: expected yellow tint, got {}",
            pix[idx + 1]
        );
        assert!(
            pix[idx + 2] > 150 && pix[idx + 2] < 230,
            "G: got {}",
            pix[idx + 2]
        );
        assert!(pix[idx + 3] < 80, "B: got {}", pix[idx + 3]);
    }

    #[test]
    fn pause_badge_leaves_top_left_alone() {
        let size = 24u32;
        let mut pix = vec![0u8; (size * size * 4) as usize];
        for px in pix.chunks_exact_mut(4) {
            px.copy_from_slice(&[255, 200, 100, 50]); // opaque orange
        }
        draw_pause_badge(&mut pix, size);
        // Pixel (0, 0) — top-left — must be untouched.
        assert_eq!(pix[0..4], [255, 200, 100, 50]);
    }

    #[test]
    fn icon_set_load_works_on_linux_only_but_synthesises_on_other_platforms() {
        // The non-Linux path synthesises 1×1 transparent pixmaps; we just
        // verify the loader doesn't panic and produces one entry per
        // size × variant.
        let set = IconSet::load();
        assert_eq!(set.base.len(), SIZES.len());
        assert_eq!(set.paused.len(), SIZES.len());
        assert_eq!(set.unreachable.len(), SIZES.len());
        for (size, bytes) in &set.base {
            assert_eq!(bytes.len(), (*size as usize) * (*size as usize) * 4);
        }
    }
}
