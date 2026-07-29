//! Dimension-safe, area-preserving resampling for panel grids.

/// Number of rows in the fixed luma ordering grid.
pub const LUMA_GRID_ROWS: u16 = 9;
/// Number of columns in the fixed luma ordering grid.
pub const LUMA_GRID_COLS: u16 = 16;

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
