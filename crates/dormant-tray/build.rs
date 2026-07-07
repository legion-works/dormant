//! Build-time rasterization of two asset families:
//!
//! 1. The brand mark (`design/web-ui/assets/mark.svg`) → ARGB32
//!    big-endian blobs at the four tray sizes (22/24/32/48 px).  These
//!    are the tray icon pixmaps.  Written as `mark_<W>.bin` and embedded
//!    via `include_bytes!`.  Variant overlays (paused badge, greying)
//!    are applied in pure Rust at runtime against the baked base
//!    pixmap — see `src/icon.rs`.
//!
//! 2. The per-item menu glyphs (`crates/dormant-tray/assets/glyphs/*.svg`)
//!    → PNG bytes at 16 px.  These ride on `ksni::StandardItem.icon_data`
//!    so the menu carries the dormant brand (green primary, blue accent)
//!    inside its structure rather than leaning on the host theme.
//!    Written as `glyph_<name>.png` and embedded via `include_bytes!`.
//!
//! Choosing build-time rasterization over a runtime `image`/`png`
//! decoder keeps the tray's runtime dependency tree tiny (no
//! `tiny-skia`/`usvg` in release) and freezes the brand assets at
//! compile time so a stale runtime cache cannot drift from the design
//! sources.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use png::Encoder;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

/// Tray sizes to rasterize the mark at.  The set is fixed at build
/// time so the runtime never allocates a pixmap — it always slices a
/// pre-baked blob.  Must match `SIZES` in `src/icon.rs`; changing one
/// without the other is a compile error from the `include_bytes!`
/// expansion.
const SIZES: &[u32] = &[22, 24, 32, 48];

/// Per-item menu glyphs to rasterize + PNG-encode at build time.
/// Order is significant only for the embedded file names; the runtime
/// looks each one up by name via `include_bytes!`.
const GLYPHS: &[&str] = &[
    "pause",
    "play",
    "display-off",
    "display-on",
    "web",
    "exit",
    "info",
];

/// Side length for rasterized glyph pixmaps.  16 px is the canonical
/// `DBusMenu` icon size — Plasma renders it at the menu's text-line height.
const GLYPH_PX: u32 = 16;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.join("..").join("..");

    let mark_svg = repo_root
        .join("design")
        .join("web-ui")
        .join("assets")
        .join("mark.svg");
    let glyphs_dir = manifest_dir.join("assets").join("glyphs");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));

    // Rerun on any design source change.
    println!("cargo:rerun-if-changed={}", mark_svg.display());
    println!("cargo:rerun-if-changed={}", glyphs_dir.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("build.rs").display()
    );

    rasterize_mark(&mark_svg, &out_dir);
    rasterize_glyphs(&glyphs_dir, &out_dir);
}

/// Rasterize the brand mark into ARGB32 big-endian blobs at every size
/// in `SIZES`.  See module docs for the encoding rationale.
fn rasterize_mark(svg_path: &Path, out_dir: &Path) {
    let svg_bytes = fs::read(svg_path)
        .unwrap_or_else(|e| panic!("read mark.svg at {}: {e}", svg_path.display()));

    let opts = Options::default();
    let tree = Tree::from_data(&svg_bytes, &opts).unwrap_or_else(|e| panic!("parse mark.svg: {e}"));

    let svg_size = tree.size();
    let svg_w = svg_size.width();
    let svg_h = svg_size.height();

    for &size in SIZES {
        let mut pixmap = Pixmap::new(size, size).expect("allocate pixmap");

        let scale_x = f64::from(size) / f64::from(svg_w);
        let scale_y = f64::from(size) / f64::from(svg_h);
        // f64 intermediate avoids the u32 → f32 precision-loss lint in
        // clippy::pedantic; the final downcast is intentional (tiny-skia
        // takes f32 transform components).
        #[allow(clippy::cast_possible_truncation)]
        let transform = Transform::from_scale(scale_x as f32, scale_y as f32);

        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // tiny-skia stores pixels as premultiplied BGRA in native byte
        // order; ksni::Icon wants ARGB32 in network byte order
        // (big-endian) and the StatusNotifierItem contract expects
        // straight (non-premultiplied) alpha.  Unpremultiply, then
        // swap each 4-byte pixel from `[B,G,R,A]` → `[A,R,G,B]`.
        let raw = pixmap.data();
        let mut argb_be = Vec::with_capacity(raw.len());
        for px in raw.chunks_exact(4) {
            argb_be.push(px[3]); // A
            argb_be.push(px[2]); // R
            argb_be.push(px[1]); // G
            argb_be.push(px[0]); // B
        }

        let out_path = out_dir.join(format!("mark_{size}.bin"));
        write_blob(&out_path, &argb_be);
    }
}

/// Rasterize each per-item menu glyph into a PNG file.  PNG (not raw
/// ARGB) because that's what `ksni::StandardItem.icon_data` expects per
/// the freedesktop `StatusNotifierItem` / `DBusMenu` spec.
fn rasterize_glyphs(glyphs_dir: &Path, out_dir: &Path) {
    let opts = Options::default();

    for name in GLYPHS {
        let svg_path = glyphs_dir.join(format!("{name}.svg"));
        let svg_bytes = fs::read(&svg_path)
            .unwrap_or_else(|e| panic!("read glyph svg {}: {e}", svg_path.display()));
        let tree = Tree::from_data(&svg_bytes, &opts)
            .unwrap_or_else(|e| panic!("parse glyph svg {name}: {e}"));

        let svg_size = tree.size();
        let svg_w = svg_size.width();
        let svg_h = svg_size.height();
        let px = GLYPH_PX;

        let mut pixmap = Pixmap::new(px, px).expect("allocate glyph pixmap");

        let scale_x = f64::from(px) / f64::from(svg_w);
        let scale_y = f64::from(px) / f64::from(svg_h);
        #[allow(clippy::cast_possible_truncation)]
        let transform = Transform::from_scale(scale_x as f32, scale_y as f32);

        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // tiny-skia hands us premultiplied BGRA; PNG wants
        // non-premultiplied RGBA.  Unpremultiply, then reorder
        // BGRA → RGBA.  Output pixels are 8-bit; we use a small
        // fixed-point multiply (×256) to round half-up.
        let raw = pixmap.data();
        let mut rgba = Vec::with_capacity(raw.len());
        for px in raw.chunks_exact(4) {
            let b = px[0];
            let g = px[1];
            let r = px[2];
            let a = px[3];
            let (r8, g8, b8) = unpremul_to_rgba8(r, g, b, a);
            rgba.push(r8);
            rgba.push(g8);
            rgba.push(b8);
            rgba.push(a);
        }

        let png_bytes = encode_png_rgba8(px, px, &rgba);
        let out_path = out_dir.join(format!("glyph_{name}.png"));
        write_blob(&out_path, &png_bytes);
    }
}

/// Unpremultiply an 8-bit BGRA pixel to straight 8-bit RGB, preserving
/// alpha.  Uses a `(ch * 255 + a/2) / max(a, 1)` round for `ch ∈ [0,
/// 255]`, `a ∈ [1, 255]`.  Returns `(r, g, b)`; alpha passes through
/// unchanged.
fn unpremul_to_rgba8(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 0 {
        return (0, 0, 0);
    }
    let a16 = u16::from(a);
    let r16 = (u16::from(r) * 255 + a16 / 2) / a16;
    let g16 = (u16::from(g) * 255 + a16 / 2) / a16;
    let b16 = (u16::from(b) * 255 + a16 / 2) / a16;
    (
        u8::try_from(r16).unwrap_or(u8::MAX),
        u8::try_from(g16).unwrap_or(u8::MAX),
        u8::try_from(b16).unwrap_or(u8::MAX),
    )
}

/// Encode `width × height` 8-bit RGBA pixels as a PNG into a `Vec<u8>`.
/// Uses the `png` crate's encoder with default filter + deflate.
fn encode_png_rgba8(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(rgba).expect("png image data");
    }
    out
}

fn write_blob(path: &Path, bytes: &[u8]) {
    let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
