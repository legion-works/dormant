//! Sparse linear-luminance scanning and cached screensaver image grids.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use dormant_core::spatial_grid::{LUMA_GRID_COLS, LUMA_GRID_ROWS, LumaGrid};
use image::ImageReader;
use thiserror::Error;

/// A source-level luminance class for video items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WearTag {
    /// Dark video class.
    Dark,
    /// Medium video class.
    Medium,
    /// Bright video class.
    Bright,
}

/// Shared process-memory catalog of scanned luma grids.
pub type LumaCatalog = Arc<RwLock<HashMap<String, LumaGrid>>>;

/// Convert one normalized sRGB channel to linear light.
#[must_use]
pub fn srgb_to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

/// Calculate Rec. 709 luminance after transfer-function conversion.
#[must_use]
pub fn linear_luma(rgb: [f32; 3]) -> f32 {
    0.2126 * srgb_to_linear(rgb[0])
        + 0.7152 * srgb_to_linear(rgb[1])
        + 0.0722 * srgb_to_linear(rgb[2])
}

/// Return the ratified flat linear-light grid for a video wear tag.
///
/// # Panics
///
/// Panics only if the fixed grid contract changes incompatibly.
#[must_use]
pub fn flat_grid_for_tag(tag: WearTag) -> LumaGrid {
    let value = match tag {
        WearTag::Dark => 0.15,
        WearTag::Medium => 0.45,
        WearTag::Bright => 0.75,
    };
    LumaGrid::new(vec![
        value;
        usize::from(LUMA_GRID_ROWS)
            * usize::from(LUMA_GRID_COLS)
    ])
    .expect("valid flat grid")
}

/// Image scanner with a path-and-modification-time cache.
pub struct LumaCache {
    entries: RwLock<HashMap<PathBuf, (SystemTime, LumaGrid)>>,
}

impl Default for LumaCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LumaCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Decode and sparsely sample an image, reusing a matching metadata entry.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata, format detection, or decoding fails.
    ///
    /// # Panics
    ///
    /// Panics only if another thread poisoned the cache lock.
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn scan_path(&self, path: &Path) -> Result<LumaGrid, LumaScanError> {
        let canonical = path.canonicalize().map_err(LumaScanError::Canonicalize)?;
        let modified = std::fs::metadata(&canonical)
            .map_err(LumaScanError::Metadata)?
            .modified()
            .map_err(LumaScanError::Modified)?;
        if let Some((_, grid)) = self
            .entries
            .read()
            .expect("luma cache lock")
            .get(&canonical)
            .filter(|(mtime, _)| *mtime == modified)
        {
            return Ok(grid.clone());
        }
        let decoded = ImageReader::open(&canonical)
            .map_err(LumaScanError::Open)?
            .with_guessed_format()
            .map_err(LumaScanError::Format)?
            .decode()
            .map_err(LumaScanError::Decode)?;
        let width = decoded.width();
        let height = decoded.height();
        let rgba = decoded.to_rgba8();
        let mut cells = Vec::with_capacity(144);
        for row in 0..usize::from(LUMA_GRID_ROWS) {
            for col in 0..usize::from(LUMA_GRID_COLS) {
                let mut sum = 0.0;
                for sample_row in 0..4 {
                    for sample_col in 0..4 {
                        let x = ((((col as f32 + (sample_col as f32 + 0.5) / 4.0) / 16.0)
                            * width as f32)
                            .floor())
                        .min(width.saturating_sub(1) as f32) as u32;
                        let y = ((((row as f32 + (sample_row as f32 + 0.5) / 4.0) / 9.0)
                            * height as f32)
                            .floor())
                        .min(height.saturating_sub(1) as f32)
                            as u32;
                        let pixel = rgba.get_pixel(x, y).0;
                        let alpha = f32::from(pixel[3]) / 255.0;
                        sum += linear_luma([
                            f32::from(pixel[0]) / 255.0 * alpha,
                            f32::from(pixel[1]) / 255.0 * alpha,
                            f32::from(pixel[2]) / 255.0 * alpha,
                        ]);
                    }
                }
                cells.push(sum / 16.0);
            }
        }
        let grid = LumaGrid::new(cells).ok_or(LumaScanError::InvalidGrid)?;
        let mut entries = self.entries.write().expect("luma cache lock");
        entries.retain(|cached_path, _| cached_path != &canonical);
        entries.insert(canonical, (modified, grid.clone()));
        Ok(grid)
    }
}

#[derive(Debug, Error)]
/// Errors encountered while reading or sampling an image.
pub enum LumaScanError {
    /// Canonical path resolution failed.
    #[error("failed to canonicalize image path: {0}")]
    Canonicalize(std::io::Error),
    /// Metadata lookup failed.
    #[error("failed to read image metadata: {0}")]
    Metadata(std::io::Error),
    /// Modification time lookup failed.
    #[error("failed to read image modification time: {0}")]
    Modified(std::io::Error),
    /// Image file opening failed.
    #[error("failed to open image: {0}")]
    Open(std::io::Error),
    /// Image format detection failed.
    #[error("failed to determine image format: {0}")]
    Format(std::io::Error),
    /// Image decoding failed.
    #[error("failed to decode image: {0}")]
    Decode(image::ImageError),
    /// The sampled result violated the fixed grid contract.
    #[error("decoded image produced an invalid luma grid")]
    InvalidGrid,
}

#[cfg(test)]
mod tests {
    use super::{LumaCache, WearTag, flat_grid_for_tag, linear_luma, srgb_to_linear};
    use dormant_core::spatial_grid::{LUMA_GRID_COLS, LUMA_GRID_ROWS};
    use image::{ImageBuffer, Rgba};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn srgb_transfer_uses_linear_segment_at_threshold() {
        assert!((srgb_to_linear(0.04045) - 0.003_130_805).abs() < 1e-7);
        assert!((srgb_to_linear(0.04046) - 0.003_131_595).abs() < 1e-7);
    }
    #[test]
    fn primary_color_luma_uses_rec709_coefficients_after_linearization() {
        assert!((linear_luma([1.0, 0.0, 0.0]) - 0.2126).abs() < 1e-7);
        assert!((linear_luma([0.0, 1.0, 0.0]) - 0.7152).abs() < 1e-7);
        assert!((linear_luma([0.0, 0.0, 1.0]) - 0.0722).abs() < 1e-7);
    }
    #[test]
    fn scanner_returns_16_by_9_row_major_grid() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fixture.png");
        ImageBuffer::from_fn(32, 18, |_, _| Rgba([128_u8, 128, 128, 255]))
            .save(&path)
            .unwrap();
        let grid = LumaCache::new().scan_path(&path).unwrap();
        assert_eq!(
            grid.cells.len(),
            usize::from(LUMA_GRID_ROWS * LUMA_GRID_COLS)
        );
        assert!(grid.cells.iter().all(|v| (*v - 0.215_860_5).abs() < 1e-5));
    }
    #[test]
    fn scanner_stratified_samples_preserve_black_and_white_halves() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("halves.png");
        ImageBuffer::from_fn(64, 36, |x, _| {
            if x < 32 {
                Rgba([0_u8, 0, 0, 255])
            } else {
                Rgba([255_u8, 255, 255, 255])
            }
        })
        .save(&path)
        .unwrap();
        let grid = LumaCache::new().scan_path(&path).unwrap();
        for row in 0..9 {
            for col in 0..16 {
                let value = grid.cells[row * 16 + col];
                if col < 8 {
                    assert!(value < 0.001);
                } else {
                    assert!((value - 1.0).abs() < 1e-6);
                }
            }
        }
    }
    #[test]
    fn cache_key_changes_when_mtime_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.png");
        ImageBuffer::from_pixel(16, 9, Rgba([10_u8, 10, 10, 255]))
            .save(&path)
            .unwrap();
        let cache = LumaCache::new();
        let first = cache.scan_path(&path).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        ImageBuffer::from_pixel(16, 9, Rgba([240_u8, 240, 240, 255]))
            .save(&path)
            .unwrap();
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_ne!(before, after);
        let second = cache.scan_path(&path).unwrap();
        assert_ne!(first, second);
    }
    #[test]
    fn video_wear_tags_map_to_flat_linear_grids() {
        for (tag, expected) in [
            (WearTag::Dark, 0.15),
            (WearTag::Medium, 0.45),
            (WearTag::Bright, 0.75),
        ] {
            let grid = flat_grid_for_tag(tag);
            assert!(
                grid.cells
                    .iter()
                    .all(|v| (*v - expected).abs() < f32::EPSILON)
            );
        }
    }
}
