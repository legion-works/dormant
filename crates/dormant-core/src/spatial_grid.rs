//! Dimension-safe, area-preserving resampling for panel grids.

/// Number of rows in the fixed luma ordering grid.
pub const LUMA_GRID_ROWS: u16 = 9;
/// Number of columns in the fixed luma ordering grid.
pub const LUMA_GRID_COLS: u16 = 16;

/// Errors encountered while reducing a raw RGBA frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridError {
    /// The frame dimensions cannot describe a valid RGBA buffer.
    InvalidDimensions,
    /// The row stride is shorter than one packed RGBA row.
    InvalidStride {
        /// Minimum packed RGBA row length.
        minimum: usize,
        /// Supplied row length.
        actual: usize,
    },
    /// The buffer length does not match its dimensions and stride.
    InvalidLength {
        /// Buffer length required by the dimensions and stride.
        expected: usize,
        /// Supplied buffer length.
        actual: usize,
    },
    /// The requested output dimensions do not match [`LumaGrid`].
    InvalidOutputDimensions {
        /// Requested output rows.
        rows: u16,
        /// Requested output columns.
        cols: u16,
    },
    /// A source frame was too small to populate every output cell.
    EmptyCell,
    /// Reduction produced a grid that failed its value invariants.
    InvalidGrid,
}

/// Reduce a packed RGBA8 frame to a 16×9 linear-light luma grid.
///
/// The transfer-function constants and Rec. 709 weights intentionally mirror
/// `dormant-render::luma`; they should move to a shared pure module if another
/// consumer needs the same conversion.
///
/// # Errors
///
/// Returns [`GridError`] when the input buffer, stride, source dimensions, or
/// requested output dimensions are malformed.
#[allow(
    clippy::cast_precision_loss,
    reason = "pixel coordinates are bounded by the supplied u32 dimensions"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "the validated normalized luma result is intentionally f32"
)]
pub fn reduce_rgba8_to_luma_grid(
    rgba: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    rows: u16,
    cols: u16,
) -> Result<LumaGrid, GridError> {
    if width == 0 || height == 0 || rows == 0 || cols == 0 {
        return Err(GridError::InvalidDimensions);
    }
    if rows != LUMA_GRID_ROWS || cols != LUMA_GRID_COLS {
        return Err(GridError::InvalidOutputDimensions { rows, cols });
    }
    let minimum_stride = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(GridError::InvalidDimensions)?;
    if stride < minimum_stride {
        return Err(GridError::InvalidStride {
            minimum: minimum_stride,
            actual: stride,
        });
    }
    let expected_length = stride
        .checked_mul(usize::try_from(height).map_err(|_| GridError::InvalidDimensions)?)
        .ok_or(GridError::InvalidDimensions)?;
    if rgba.len() != expected_length {
        return Err(GridError::InvalidLength {
            expected: expected_length,
            actual: rgba.len(),
        });
    }

    let cell_count = usize::from(rows) * usize::from(cols);
    let mut sums = vec![0.0_f64; cell_count];
    let mut counts = vec![0_usize; cell_count];
    let width_usize = usize::try_from(width).map_err(|_| GridError::InvalidDimensions)?;
    let height_usize = usize::try_from(height).map_err(|_| GridError::InvalidDimensions)?;
    for y in 0..height_usize {
        for x in 0..width_usize {
            let cell_row = (y as u64 * u64::from(rows) / u64::from(height)) as usize;
            let cell_col = (x as u64 * u64::from(cols) / u64::from(width)) as usize;
            let cell = cell_row * usize::from(cols) + cell_col;
            let offset = y * stride + x * 4;
            let alpha = f32::from(rgba[offset + 3]) / 255.0;
            let red = f32::from(rgba[offset]) / 255.0 * alpha;
            let green = f32::from(rgba[offset + 1]) / 255.0 * alpha;
            let blue = f32::from(rgba[offset + 2]) / 255.0 * alpha;
            let srgb_to_linear = |channel: f32| {
                if channel <= 0.04045 {
                    channel / 12.92
                } else {
                    ((channel + 0.055) / 1.055).powf(2.4)
                }
            };
            sums[cell] += f64::from(
                0.2126 * srgb_to_linear(red)
                    + 0.7152 * srgb_to_linear(green)
                    + 0.0722 * srgb_to_linear(blue),
            );
            counts[cell] += 1;
        }
    }
    let cells = sums
        .into_iter()
        .zip(counts)
        .map(|(sum, count)| {
            if count == 0 {
                return Err(GridError::EmptyCell);
            }
            Ok((sum / count as f64) as f32)
        })
        .collect::<Result<Vec<_>, _>>()?;
    LumaGrid::new(cells).ok_or(GridError::InvalidGrid)
}

/// A fixed-size, row-major luma grid.
#[derive(Debug, Clone, PartialEq)]
pub struct LumaGrid {
    /// Normalized luma values in row-major order.
    pub cells: Vec<f32>,
}

impl LumaGrid {
    /// Construct a luma grid after checking its fixed dimensions and values.
    ///
    /// Non-finite or out-of-range values reject the whole grid with `None`.
    #[must_use]
    pub fn new(cells: Vec<f32>) -> Option<Self> {
        let expected = usize::from(LUMA_GRID_ROWS) * usize::from(LUMA_GRID_COLS);
        (cells.len() == expected
            && cells
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)))
        .then_some(Self { cells })
    }
}

/// A finite row-major heat grid with explicit dimensions.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatGrid {
    /// Number of rows represented by `cells`.
    pub rows: u16,
    /// Number of columns represented by `cells`.
    pub cols: u16,
    /// Heat values in row-major order.
    pub cells: Vec<f32>,
}

impl HeatGrid {
    /// Construct a heat grid after checking dimensions and values.
    #[must_use]
    pub fn new(rows: u16, cols: u16, cells: Vec<f32>) -> Option<Self> {
        let expected = checked_cell_count(rows, cols)?;
        (cells.len() == expected && cells.iter().all(|value| value.is_finite())).then_some(Self {
            rows,
            cols,
            cells,
        })
    }
}

fn checked_cell_count(rows: u16, cols: u16) -> Option<usize> {
    (rows != 0 && cols != 0).then_some(usize::from(rows).checked_mul(usize::from(cols))?)
}

/// Resample row-major cell averages using normalized rectangle overlap.
///
/// The operation preserves uniform fields and the mean intensity. Invalid
/// dimensions, lengths, or non-finite inputs return `None`.
/// The computation is `O(N_src * N_dst)`; at the 64×64 bound this is at most
/// roughly 16.8 million overlap checks for one resampling operation.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "grid dimensions are at most u16, so normalized coordinates are exact"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "the output contract is f32 and narrowing the finite weighted average is intentional"
)]
pub fn resample_area(
    values: &[f32],
    src_rows: u16,
    src_cols: u16,
    dst_rows: u16,
    dst_cols: u16,
) -> Option<Vec<f32>> {
    let src_count = checked_cell_count(src_rows, src_cols)?;
    let dst_count = checked_cell_count(dst_rows, dst_cols)?;
    if values.len() != src_count || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    if src_rows == dst_rows && src_cols == dst_cols {
        return Some(values.to_vec());
    }

    let src_rows_f = f64::from(src_rows);
    let src_cols_f = f64::from(src_cols);
    let dst_rows_f = f64::from(dst_rows);
    let dst_cols_f = f64::from(dst_cols);
    let mut output = Vec::with_capacity(dst_count);
    for dst_row in 0..usize::from(dst_rows) {
        let dst_top = dst_row as f64 / dst_rows_f;
        let dst_bottom = (dst_row + 1) as f64 / dst_rows_f;
        for dst_col in 0..usize::from(dst_cols) {
            let dst_left = dst_col as f64 / dst_cols_f;
            let dst_right = (dst_col + 1) as f64 / dst_cols_f;
            let mut weighted = 0.0;
            for src_row in 0..usize::from(src_rows) {
                let src_top = src_row as f64 / src_rows_f;
                let src_bottom = (src_row + 1) as f64 / src_rows_f;
                let row_overlap = (dst_bottom.min(src_bottom) - dst_top.max(src_top)).max(0.0);
                if row_overlap == 0.0 {
                    continue;
                }
                for src_col in 0..usize::from(src_cols) {
                    let src_left = src_col as f64 / src_cols_f;
                    let src_right = (src_col + 1) as f64 / src_cols_f;
                    let col_overlap = (dst_right.min(src_right) - dst_left.max(src_left)).max(0.0);
                    weighted += f64::from(values[src_row * usize::from(src_cols) + src_col])
                        * row_overlap
                        * col_overlap;
                }
            }
            let area = (dst_bottom - dst_top) * (dst_right - dst_left);
            output.push((weighted / area) as f32);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn resample_area_identity_is_exact() {
        let values: Vec<f32> = (0_u16..144).map(|value| f32::from(value) / 10.0).collect();
        assert_eq!(resample_area(&values, 9, 16, 9, 16), Some(values));
    }

    #[test]
    fn resample_area_uniform_grid_stays_uniform_at_any_valid_size() {
        for (rows, cols) in [(4_u16, 4_u16), (9, 16), (64, 64)] {
            let values = vec![0.375; usize::from(rows) * usize::from(cols)];
            let result = resample_area(&values, rows, cols, 9, 16).unwrap();
            assert!(result.iter().all(|value| (*value - 0.375).abs() < 1e-6));
        }
    }

    #[test]
    fn luma_grid_rejects_non_finite_and_out_of_range_values() {
        let valid = vec![0.5; usize::from(LUMA_GRID_ROWS) * usize::from(LUMA_GRID_COLS)];
        assert!(LumaGrid::new(valid.clone()).is_some());

        let mut too_high = valid.clone();
        too_high[0] = 1.1;
        assert!(LumaGrid::new(too_high).is_none());

        let mut negative = valid.clone();
        negative[0] = -0.1;
        assert!(LumaGrid::new(negative).is_none());

        let mut nan = valid;
        nan[0] = f32::NAN;
        assert!(LumaGrid::new(nan).is_none());
    }

    #[test]
    fn reduce_rgba8_maps_black_white_and_primary_colors() {
        let cases = [
            ([0, 0, 0, 255], 0.0),
            ([255, 255, 255, 255], 1.0),
            ([255, 0, 0, 255], 0.2126),
            ([0, 255, 0, 255], 0.7152),
            ([0, 0, 255, 255], 0.0722),
        ];
        for (pixel, expected) in cases {
            let rgba = pixel.repeat(16 * 9);
            let grid = reduce_rgba8_to_luma_grid(&rgba, 16, 9, 16 * 4, 9, 16).unwrap();
            assert!(
                grid.cells
                    .iter()
                    .all(|value| (*value - expected).abs() < 1e-6)
            );
        }
    }

    #[test]
    fn reduce_rgba8_matches_srgb_branch_boundary() {
        let low = [10_u8, 10, 10, 255];
        let high = [11_u8, 11, 11, 255];
        for pixel in [low, high] {
            let rgba = pixel.repeat(16 * 9);
            let grid = reduce_rgba8_to_luma_grid(&rgba, 16, 9, 64, 9, 16).unwrap();
            let channel = f32::from(rgba[0]) / 255.0;
            let expected = if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            };
            assert!((grid.cells[0] - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn reduce_rgba8_ignores_padded_stride_bytes() {
        let mut rgba = vec![0_u8; 68 * 9];
        for row in 0..9 {
            for col in 0..16 {
                let offset = row * 68 + col * 4;
                rgba[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
            rgba[row * 68 + 64..row * 68 + 68].fill(0);
        }
        let grid = reduce_rgba8_to_luma_grid(&rgba, 16, 9, 68, 9, 16).unwrap();
        assert!(grid.cells.iter().all(|value| (*value - 1.0).abs() < 1e-6));
    }

    #[test]
    fn reduce_rgba8_covers_non_divisible_source_dimensions() {
        let mut rgba = vec![0_u8; 17 * 10 * 4];
        for row in 0..10 {
            for col in 0..17 {
                let offset = (row * 17 + col) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let grid = reduce_rgba8_to_luma_grid(&rgba, 17, 10, 17 * 4, 9, 16).unwrap();
        assert!(grid.cells.iter().all(|value| (*value - 1.0).abs() < 1e-6));
    }

    #[test]
    fn reduce_rgba8_averages_linear_luma_not_gamma_channels() {
        let mut rgba = vec![0_u8; 32 * 18 * 4];
        for channel in rgba.chunks_exact_mut(4) {
            channel[3] = 255;
        }
        rgba[3] = 255;
        rgba[0..3].fill(255);
        let grid = reduce_rgba8_to_luma_grid(&rgba, 32, 18, 128, 9, 16).unwrap();
        assert!((grid.cells[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn reduce_rgba8_composites_alpha_over_black_before_transfer() {
        let pixel = [255, 0, 0, 128];
        let rgba = pixel.repeat(16 * 9);
        let grid = reduce_rgba8_to_luma_grid(&rgba, 16, 9, 64, 9, 16).unwrap();
        let alpha = 128.0 / 255.0;
        let expected = 0.2126 * (alpha + 0.055_f32).powf(2.4) / 1.055_f32.powf(2.4);
        assert!((grid.cells[0] - expected).abs() < 1e-6);
    }

    #[test]
    fn reduce_rgba8_rejects_malformed_length_and_stride() {
        let rgba = vec![0_u8; 16 * 9 * 4];
        assert!(reduce_rgba8_to_luma_grid(&rgba[..rgba.len() - 1], 16, 9, 64, 9, 16).is_err());
        assert!(reduce_rgba8_to_luma_grid(&rgba, 16, 9, 63, 9, 16).is_err());
        assert!(reduce_rgba8_to_luma_grid(&rgba, 16, 9, 64, 0, 16).is_err());
    }

    #[test]
    fn reduce_rgba8_produces_deterministic_16_by_9_output() {
        let mut rgba = vec![0_u8; 16 * 9 * 4];
        for row in 0..9 {
            for col in 0..16 {
                let offset = (row * 16 + col) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[
                    u8::try_from(col * 16).unwrap(),
                    u8::try_from(row * 16).unwrap(),
                    0,
                    255,
                ]);
            }
        }
        let grid = reduce_rgba8_to_luma_grid(&rgba, 16, 9, 64, 9, 16).unwrap();
        assert_eq!(grid.cells.len(), 144);
        assert_eq!(
            grid.cells,
            reduce_rgba8_to_luma_grid(&rgba, 16, 9, 64, 9, 16)
                .unwrap()
                .cells
        );
        assert!(grid.cells[0] < grid.cells[143]);
    }

    proptest! {
        #[test]
        fn resample_area_outputs_finite_values_inside_input_range(
            (src_rows, src_cols, values, dst_rows, dst_cols) in
                (1u16..=64, 1u16..=64).prop_flat_map(|(rows, cols)| {
                    let len = usize::from(rows) * usize::from(cols);
                    (Just(rows), Just(cols), prop::collection::vec(-1000.0f32..1000.0, len..=len), 1u16..=64, 1u16..=64)
                }),
        ) {
            let output = resample_area(&values, src_rows, src_cols, dst_rows, dst_cols).unwrap();
            let min = values.iter().copied().fold(f32::INFINITY, f32::min);
            let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            prop_assert!(output.iter().all(|value| value.is_finite() && *value >= min - 1e-4 && *value <= max + 1e-4));
        }

        #[test]
        fn resample_area_conserves_mean_intensity(
            (src_rows, src_cols, values, dst_rows, dst_cols) in
                (1u16..=64, 1u16..=64).prop_flat_map(|(rows, cols)| {
                    let len = usize::from(rows) * usize::from(cols);
                    (Just(rows), Just(cols), prop::collection::vec(0.0f32..1.0, len..=len), 1u16..=64, 1u16..=64)
                }),
        ) {
            let output = resample_area(&values, src_rows, src_cols, dst_rows, dst_cols).unwrap();
            let source_count = u32::try_from(values.len()).unwrap_or(u32::MAX);
            let output_count = u32::try_from(output.len()).unwrap_or(u32::MAX);
            let source_mean = values.iter().map(|v| f64::from(*v)).sum::<f64>() / f64::from(source_count);
            let output_mean = output.iter().map(|v| f64::from(*v)).sum::<f64>() / f64::from(output_count);
            prop_assert!((source_mean - output_mean).abs() < 1e-4);
        }
    }
}
