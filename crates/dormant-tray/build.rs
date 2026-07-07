//! Build-time rasterization of `design/web-ui/assets/mark.svg` into ARGB32
//! big-endian blobs at the four tray sizes (22/24/32/48 px).
//!
//! The blobs are written into `OUT_DIR/mark_<W>.bin` and embedded at runtime
//! via `include_bytes!`. Variant overlays (paused badge, greyed tint) are
//! applied in pure-Rust against the baked base pixmap at runtime — see
//! `src/icon.rs`.  Choosing build-time rasterization over a runtime
//! `image`/`png` decoder keeps the tray's runtime dependency tree tiny
//! (no `tiny-skia`/`usvg` in release) and freezes the brand asset at compile
//! time so a stale runtime cache cannot drift from the design source.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

/// Tray sizes to rasterize.  The set is fixed at build time so the runtime
/// never allocates a pixmap — it always slices a pre-baked blob.
const SIZES: &[u32] = &[22, 24, 32, 48];

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // The mark asset lives at <repo>/design/web-ui/assets/mark.svg; the tray
    // crate sits at <repo>/crates/dormant-tray/, so walk up two directories.
    let svg_path = manifest_dir
        .join("..")
        .join("..")
        .join("design")
        .join("web-ui")
        .join("assets")
        .join("mark.svg");

    println!("cargo:rerun-if-changed={}", svg_path.display());
    // Rerun if the build script itself changes (e.g. new size added).
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("build.rs").display()
    );

    let svg_bytes = fs::read(&svg_path)
        .unwrap_or_else(|e| panic!("read mark.svg at {}: {e}", svg_path.display()));

    let opts = Options::default();
    let tree = Tree::from_data(&svg_bytes, &opts).unwrap_or_else(|e| panic!("parse mark.svg: {e}"));

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));

    let svg_size = tree.size();
    let svg_w = svg_size.width();
    let svg_h = svg_size.height();

    for &size in SIZES {
        let mut pixmap = Pixmap::new(size, size).expect("allocate pixmap");

        let scale_x = f64::from(size) / f64::from(svg_w);
        let scale_y = f64::from(size) / f64::from(svg_h);
        // f64 intermediate avoids the u32 → f32 precision-loss lint in
        // clippy::pedantic; the final downcast is intentional (tiny-skia takes
        // f32 transform components).
        #[allow(clippy::cast_possible_truncation)]
        let transform = Transform::from_scale(scale_x as f32, scale_y as f32);

        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // tiny-skia stores pixels as premultiplied BGRA in native byte order;
        // ksni::Icon wants ARGB32 in network byte order (big-endian) and the
        // StatusNotifierItem contract expects straight (non-premultiplied)
        // alpha.  Unpremultiply, then swap each 4-byte pixel from
        // `[B,G,R,A]` → `[A,R,G,B]`.
        let raw = pixmap.data();
        let mut argb_be = Vec::with_capacity(raw.len());
        for px in raw.chunks_exact(4) {
            // Straight BGRA → ARGB big-endian.
            argb_be.push(px[3]); // A
            argb_be.push(px[2]); // R
            argb_be.push(px[1]); // G
            argb_be.push(px[0]); // B
        }

        let out_path = out_dir.join(format!("mark_{size}.bin"));
        write_blob(&out_path, &argb_be);
    }
}

fn write_blob(path: &Path, bytes: &[u8]) {
    let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
