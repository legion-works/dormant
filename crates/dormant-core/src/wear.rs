//! Pure wear model — no I/O; the tracker in dormantd owns files.
//!
//! [`WearLedger`] tracks per-cell on-time (weighted by brightness) across a
//! coarse grid overlaid on a panel, so callers can render a heat map and
//! reason about uneven burn-in risk. Everything here is pure data + math —
//! reading/writing ledgers to disk, sampling [`crate::traits::PanelState`] on
//! a schedule, and deciding when to persist are all owned by the tracker in
//! `dormantd`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::traits::PanelState;

/// Current on-disk schema version for [`WearLedger`].
///
/// Bump this whenever a change to `WearLedger`'s shape would break
/// deserialization of an older ledger file; the loader (see the tracker in
/// `dormantd`) branches on this field to decide whether to migrate or reset.
pub const WEAR_SCHEMA_VERSION: u32 = 1;

/// Coarse panel technology classification.
///
/// Used to pick technology-appropriate wear heuristics (e.g. QD-OLED and
/// W-OLED age differently under the same brightness/dwell profile). Falls
/// back to [`PanelType::Unknown`] whenever the identity source can't tell —
/// a missing classification should never block wear tracking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelType {
    /// White-OLED (WRGB) panel.
    Woled,
    /// Quantum-dot OLED panel.
    QdOled,
    /// Panel technology could not be determined.
    #[default]
    Unknown,
}

/// Accumulated wear for a single grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WearCell {
    /// Brightness-weighted on-hours attributed to this cell.
    pub wear_hours: f64,
}

/// Stable identity for the display a ledger belongs to.
///
/// `key` is the sanitized, filesystem- and config-safe form (see
/// [`sanitize_identity_key`]) used to key [`WearHandle`] and name ledger
/// files; `display_name` is the human-readable label shown in UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WearIdentity {
    /// Sanitized identity key, stable across restarts for the same panel.
    pub key: String,
    /// Human-readable display name.
    pub display_name: String,
    /// The `[displays.*]` config id this ledger is attributed to, when
    /// known.  Absent for ledgers created before this field was added
    /// (v0.1.0 ledgers) or for orphan ledgers with no matching config
    /// display.  The frontend joins on this field first, then falls back
    /// to `display_name` for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_display_id: Option<String>,
}

/// Per-display wear ledger: a grid of [`WearCell`]s plus bookkeeping.
///
/// Pure data + math — no I/O. The tracker in `dormantd` owns reading,
/// writing, and periodic sampling; this type only knows how to accumulate
/// and reshape wear given the numbers it's handed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearLedger {
    /// On-disk schema version — see [`WEAR_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Identity of the display this ledger tracks.
    pub identity: WearIdentity,
    /// Panel technology, if known.
    pub panel_type: PanelType,
    /// Number of grid rows.
    pub grid_rows: u16,
    /// Number of grid columns.
    pub grid_cols: u16,
    /// Row-major grid of per-cell wear, length `grid_rows * grid_cols`.
    pub cells: Vec<WearCell>,
    /// Panel-mean brightness-weighted on-hours: the sum of cell values divided
    /// by the cell count, rather than the sum across cells.
    pub total_on_hours: f64,
    /// Optional operator-supplied prior usage, in hours, seeded at ledger
    /// creation for panels that weren't new when tracking started.
    pub seeded_usage_hours: Option<u32>,
    /// Number of `attribute_uniform` samples applied to this ledger.
    pub sample_count: u64,
    /// Epoch seconds of the most recent sample, if any.
    pub last_sample_at_epoch_s: Option<u64>,
    /// Epoch seconds of the most recent long-dwell observation, if any.
    ///
    /// Observed-only: starts `None` and is never inferred or backfilled.
    pub last_long_dwell_epoch_s: Option<u64>,
    /// Epoch seconds this ledger was created — the "assume-healthy" baseline
    /// advisories are measured relative to.
    pub advisory_baseline_epoch_s: u64,
}

/// Failure returned when spatial luma does not match the ledger grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpatialAttributionError {
    /// The supplied luma length differs from the ledger cell count.
    #[error("spatial luma length mismatch: expected {expected}, got {actual}")]
    LengthMismatch {
        /// Required number of luma cells.
        expected: usize,
        /// Supplied number of luma cells.
        actual: usize,
    },
}

impl WearLedger {
    /// Create a new, all-zero ledger for `identity` with a `rows` × `cols`
    /// grid, baselined at `now_epoch_s`.
    #[must_use]
    pub fn new(
        identity: WearIdentity,
        panel_type: PanelType,
        rows: u16,
        cols: u16,
        now_epoch_s: u64,
    ) -> Self {
        let cell_count = usize::from(rows) * usize::from(cols);
        Self {
            schema_version: WEAR_SCHEMA_VERSION,
            identity,
            panel_type,
            grid_rows: rows,
            grid_cols: cols,
            cells: vec![WearCell { wear_hours: 0.0 }; cell_count],
            total_on_hours: 0.0,
            seeded_usage_hours: None,
            sample_count: 0,
            last_sample_at_epoch_s: None,
            last_long_dwell_epoch_s: None,
            advisory_baseline_epoch_s: now_epoch_s,
        }
    }

    /// Attribute `span` of uniform on-time, weighted by `brightness_norm`
    /// (clamped to `0.0..=1.0`), to every cell in the grid and to the
    /// running total.
    ///
    /// "Uniform" because this models content that lights the whole panel
    /// evenly (desktop UI, full-screen video); per-region attribution is a
    /// later extension.
    pub fn attribute_uniform(&mut self, span: Duration, brightness_norm: f64) {
        let n = finite_clamp(brightness_norm);
        let h = span.as_secs_f64() / 3600.0 * n;
        for c in &mut self.cells {
            c.wear_hours += h;
        }
        self.total_on_hours += h;
        self.sample_count += 1;
    }

    /// Attribute a spatial luma sample while preserving the ledger shape.
    ///
    /// # Errors
    ///
    /// Returns [`SpatialAttributionError::LengthMismatch`] when `luma` does
    /// not have one value for every ledger cell; no state is changed then.
    /// Non-finite brightness and luma values are treated as zero before the
    /// finite values are clamped to `0.0..=1.0`.
    #[allow(
        clippy::cast_precision_loss,
        reason = "ledger grids are u16-sized; conversion is exact for supported dimensions"
    )]
    pub fn attribute_spatial(
        &mut self,
        span: Duration,
        brightness_norm: f64,
        luma: &[f32],
    ) -> Result<(), SpatialAttributionError> {
        if luma.len() != self.cells.len() {
            return Err(SpatialAttributionError::LengthMismatch {
                expected: self.cells.len(),
                actual: luma.len(),
            });
        }
        let brightness = finite_clamp(brightness_norm);
        let span_hours = span.as_secs_f64() / 3600.0;
        let mut total = 0.0;
        for (cell, value) in self.cells.iter_mut().zip(luma) {
            let contribution = span_hours * brightness * finite_clamp(f64::from(*value));
            cell.wear_hours += contribution;
            total += contribution;
        }
        if !self.cells.is_empty() {
            let cell_count = u32::try_from(self.cells.len()).unwrap_or(u32::MAX);
            self.total_on_hours += total / f64::from(cell_count);
        }
        self.sample_count += 1;
        Ok(())
    }

    /// Zero-max normalized wear per cell, in row-major order, each in
    /// `0.0..=1.0`.
    ///
    /// Divides each cell by the **maximum** cell value rather than the
    /// min-max range — a uniformly-worn panel (`min == max > 0`) would
    /// otherwise collapse to all-zero heat (issue #108), indistinguishable
    /// from a fresh / unsampled ledger.
    ///
    /// Only an all-zero grid (`max <= 0.0`) yields all-zero heat; that
    /// is the genuine "no data" case, not a uniform one.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "heat_map is a display-precision output (0.0..=1.0); f64->f32 narrowing here is intentional, not an accumulator"
    )]
    pub fn heat_map(&self) -> Vec<f32> {
        let max = self
            .cells
            .iter()
            .map(|c| c.wear_hours)
            .fold(0.0_f64, f64::max);
        if max <= 0.0 {
            return vec![0.0; self.cells.len()];
        }
        self.cells
            .iter()
            .map(|c| (c.wear_hours / max) as f32)
            .collect()
    }

    /// Resize the grid to `rows` × `cols`, redistributing existing wear by
    /// spatial density rather than flattening the total evenly.
    ///
    /// Each old cell is treated as a unit-area rectangle holding a uniform
    /// wear density (`wear_hours` per unit area); the new grid is laid over
    /// the same `[0, rows) × [0, cols)` unit-normalized rectangle and each
    /// new cell's wear is the area-weighted overlap integral against every
    /// old cell it intersects. This conserves `total_on_hours` exactly (by
    /// construction — overlap areas partition the old cells) while
    /// preserving *where* the wear was, which flat `total / (rows * cols)`
    /// redistribution would destroy.
    pub fn resize_grid(&mut self, rows: u16, cols: u16) {
        let old_rows = usize::from(self.grid_rows);
        let old_cols = usize::from(self.grid_cols);
        let new_rows = usize::from(rows);
        let new_cols = usize::from(cols);

        if old_rows == 0 || old_cols == 0 || new_rows == 0 || new_cols == 0 {
            self.grid_rows = rows;
            self.grid_cols = cols;
            self.cells = vec![WearCell { wear_hours: 0.0 }; new_rows * new_cols];
            // Empty grid ⇒ no wear representable: keep the "cells sum ≈
            // total" invariant intact rather than leaving a stale total
            // that no cell can account for.
            self.total_on_hours = 0.0;
            return;
        }

        // Old cell (r, c) occupies the unit-normalized rectangle
        // [c/old_cols, (c+1)/old_cols) x [r/old_rows, (r+1)/old_rows), and
        // holds wear_hours as its total content (density = wear_hours,
        // since old cell area in the unit-normalized space is
        // (1/old_cols) * (1/old_rows)).
        // Loop indices are all bounded by u16 grid dimensions, so the
        // usize -> u32 -> f64 conversion chain below is exact (no
        // `as`-cast precision loss).
        let idx_f64 = |i: usize| -> f64 { f64::from(u32::try_from(i).unwrap_or(u32::MAX)) };

        let row_scale_old = 1.0 / idx_f64(old_rows);
        let col_scale_old = 1.0 / idx_f64(old_cols);
        let row_scale_new = 1.0 / idx_f64(new_rows);
        let col_scale_new = 1.0 / idx_f64(new_cols);

        let mut new_cells = vec![WearCell { wear_hours: 0.0 }; new_rows * new_cols];

        for old_r in 0..old_rows {
            let old_top = idx_f64(old_r) * row_scale_old;
            let old_bottom = old_top + row_scale_old;
            for old_c in 0..old_cols {
                let old_left = idx_f64(old_c) * col_scale_old;
                let old_right = old_left + col_scale_old;
                let old_wear = self.cells[old_r * old_cols + old_c].wear_hours;
                if old_wear == 0.0 {
                    continue;
                }
                let old_area = row_scale_old * col_scale_old;

                for new_r in 0..new_rows {
                    let new_top = idx_f64(new_r) * row_scale_new;
                    let new_bottom = new_top + row_scale_new;
                    let row_overlap = (old_bottom.min(new_bottom) - old_top.max(new_top)).max(0.0);
                    if row_overlap <= 0.0 {
                        continue;
                    }
                    for new_c in 0..new_cols {
                        let new_left = idx_f64(new_c) * col_scale_new;
                        let new_right = new_left + col_scale_new;
                        let col_overlap =
                            (old_right.min(new_right) - old_left.max(new_left)).max(0.0);
                        if col_overlap <= 0.0 {
                            continue;
                        }
                        let overlap_area = row_overlap * col_overlap;
                        let fraction = overlap_area / old_area;
                        new_cells[new_r * new_cols + new_c].wear_hours += old_wear * fraction;
                    }
                }
            }
        }

        self.grid_rows = rows;
        self.grid_cols = cols;
        self.cells = new_cells;
    }
}

/// Normalize a controller's native brightness readback to `0.0..=1.0` given
/// `native_max` (the controller's top-of-scale value, e.g. `100` for DDC/CI,
/// `50` for Samsung port-1516). Falls back to `fallback` when the panel has
/// no brightness readback at all; always clamped to `0.0..=1.0`.
#[must_use]
pub fn brightness_norm(panel: &PanelState, native_max: u16, fallback: f64) -> f64 {
    panel
        .brightness
        .map_or(fallback, |b| f64::from(b) / f64::from(native_max))
        .clamp(0.0, 1.0)
}

fn finite_clamp(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Shared, lock-guarded map of wear ledgers keyed by config `DisplayId`
/// string, handed to whichever components (tracker, IPC handlers) need
/// concurrent read/write access to live ledgers.
pub type WearHandle = Arc<RwLock<HashMap<String, WearLedger>>>;

/// Seconds elapsed since the "effective dwell reference point":
/// `now - max(last_long_dwell_epoch_s.unwrap_or(0), advisory_baseline_epoch_s)`.
///
/// This is the single source of truth for the compensation-advisory
/// formula — both [`advisory_active`] and [`hours_since_effective_dwell`]
/// are defined in terms of it, and both `dormantd::wear_tracker::tick` and
/// `dormant_web::routes::wear::summarize` call through here instead of
/// each independently re-deriving the arithmetic (see review finding W1 —
/// the two crates used to duplicate this formula with no shared
/// implementation and no cross-crate test proving they agreed).
fn seconds_since_effective_dwell(
    last_long_dwell_epoch_s: Option<u64>,
    advisory_baseline_epoch_s: u64,
    now_epoch_s: u64,
) -> u64 {
    let observed = last_long_dwell_epoch_s.unwrap_or(0);
    let reference = observed.max(advisory_baseline_epoch_s);
    now_epoch_s.saturating_sub(reference)
}

/// Whole hours since the effective dwell reference point — see
/// `seconds_since_effective_dwell` above. Shared by the daemon's
/// `CompensationAdvisory` event (`hours_since_long_dwell`) and the web
/// `WearSummary::hours_since_long_dwell` field, so both are always
/// computed the same way.
#[must_use]
pub fn hours_since_effective_dwell(
    last_long_dwell_epoch_s: Option<u64>,
    advisory_baseline_epoch_s: u64,
    now_epoch_s: u64,
) -> u64 {
    seconds_since_effective_dwell(
        last_long_dwell_epoch_s,
        advisory_baseline_epoch_s,
        now_epoch_s,
    ) / 3600
}

/// `true` once longer than `advisory_after` has elapsed since the
/// effective dwell reference point (see `seconds_since_effective_dwell`
/// above).
///
/// The single shared advisory-latch condition: `dormantd::wear_tracker::tick`
/// uses it to decide when to fire `TrackerAction::EmitAdvisory` (latched —
/// only fires once per dwell-reset cycle, latch state lives in the
/// tracker), and `dormant_web::routes::wear::summarize` uses it to
/// recompute the `advisory` flag statelessly on every `GET /api/wear`
/// request, independent of whether the client saw the WS event. A future
/// change to the formula (e.g. a v2 tweak) only has to happen here for
/// both call sites to follow.
#[must_use]
pub fn advisory_active(
    last_long_dwell_epoch_s: Option<u64>,
    advisory_baseline_epoch_s: u64,
    advisory_after: Duration,
    now_epoch_s: u64,
) -> bool {
    seconds_since_effective_dwell(
        last_long_dwell_epoch_s,
        advisory_baseline_epoch_s,
        now_epoch_s,
    ) > advisory_after.as_secs()
}

/// Sanitize an arbitrary identity string (e.g. a DDC EDID digest or a
/// Samsung IP) into a key safe for filenames and config: lowercased,
/// restricted to `[a-z0-9._-]`, every other character replaced with `-`,
/// truncated to at most 64 characters.
#[must_use]
pub fn sanitize_identity_key(key: &str) -> String {
    let sanitized: String = key
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "test literals are exact by construction (0.0 baseline, min/max-normalized 0.0/0.5/1.0) — no accumulated float error to tolerate"
)]
mod tests {
    use super::*;
    use crate::traits::PanelState;
    use std::time::Duration;

    fn ident() -> WearIdentity {
        WearIdentity {
            key: "ddc:AOC:AG326UZD:XK2R9JA000013".into(),
            display_name: "monitor".into(),
            config_display_id: None,
        }
    }

    #[test]
    fn new_ledger_zero_grid_and_baseline() {
        let l = WearLedger::new(ident(), PanelType::QdOled, 9, 16, 1_000);
        assert_eq!(l.schema_version, WEAR_SCHEMA_VERSION);
        assert_eq!(l.cells.len(), 9 * 16);
        assert!(l.cells.iter().all(|c| c.wear_hours == 0.0));
        assert_eq!(l.total_on_hours, 0.0);
        assert_eq!(l.last_long_dwell_epoch_s, None); // observed-only, starts None
        assert_eq!(l.advisory_baseline_epoch_s, 1_000); // assume-healthy baseline
    }

    #[test]
    fn attribute_uniform_advances_all_cells_and_total() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 2, 2, 0);
        l.attribute_uniform(Duration::from_secs(3600), 0.5);
        for c in &l.cells {
            assert!((c.wear_hours - 0.5).abs() < 1e-9);
        }
        assert!((l.total_on_hours - 0.5).abs() < 1e-9);
        assert_eq!(l.sample_count, 1);
    }

    #[test]
    fn attribute_clamps_brightness_norm() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 1, 0);
        l.attribute_uniform(Duration::from_secs(3600), 7.5); // clamped to 1.0
        assert!((l.cells[0].wear_hours - 1.0).abs() < 1e-9);
        l.attribute_uniform(Duration::from_secs(3600), -3.0); // clamped to 0.0
        assert!((l.cells[0].wear_hours - 1.0).abs() < 1e-9);
    }

    #[test]
    fn attribute_uniform_nan_brightness_is_zero() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 2, 0);
        l.attribute_uniform(Duration::from_secs(3600), f64::NAN);
        assert!(l.cells.iter().all(|cell| cell.wear_hours == 0.0));
        assert_eq!(l.total_on_hours, 0.0);
        assert_eq!(l.sample_count, 1);
    }

    #[test]
    fn spatial_attribution_multiplies_span_brightness_and_each_cell_luma() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 2, 2, 0);
        l.attribute_spatial(Duration::from_secs(3600), 0.5, &[0.2, 0.4, 0.6, 0.8])
            .unwrap();
        let values: Vec<f64> = l.cells.iter().map(|cell| cell.wear_hours).collect();
        for (actual, expected) in values.into_iter().zip([0.1, 0.2, 0.3, 0.4]) {
            assert!((actual - expected).abs() < 1e-6);
        }
        assert!((l.total_on_hours - 0.25).abs() < 1e-6);
        assert_eq!(l.sample_count, 1);
    }

    #[test]
    fn spatial_attribution_rejects_length_mismatch_without_mutation() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 2, 2, 0);
        let before = l.clone();
        assert!(matches!(
            l.attribute_spatial(Duration::from_secs(3600), 0.5, &[1.0]),
            Err(SpatialAttributionError::LengthMismatch { .. })
        ));
        assert_eq!(l, before);
    }

    #[test]
    fn spatial_attribution_zero_luma_increments_sample_but_not_wear() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 2, 0);
        l.attribute_spatial(Duration::from_secs(3600), 1.0, &[0.0, 0.0])
            .unwrap();
        assert!(l.cells.iter().all(|cell| cell.wear_hours == 0.0));
        assert_eq!(l.total_on_hours, 0.0);
        assert_eq!(l.sample_count, 1);
    }

    #[test]
    fn heat_map_min_max_normalized_and_flat_grid_defined() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 3, 0);
        l.cells[0].wear_hours = 0.0;
        l.cells[1].wear_hours = 5.0;
        l.cells[2].wear_hours = 10.0;
        assert_eq!(l.heat_map(), vec![0.0, 0.5, 1.0]);
        // all-equal grid: defined flat output (0.0 everywhere), not NaN
        let flat = WearLedger::new(ident(), PanelType::Unknown, 1, 3, 0);
        assert_eq!(flat.heat_map(), vec![0.0, 0.0, 0.0]);
    }

    // ── T11 (#108): zero-max normalization ──────────────────────────────────
    //
    // Issue #108: the wear heat map used min-max normalization, so a
    // uniformly-worn panel (`min == max`) collapsed to all-zero heat —
    // indistinguishable from a panel with no recorded exposure at all.
    // The fix divides by the maximum cell hours instead of the range:
    //
    //   - uniform non-zero exposure  → every cell at 1.0 (visible hot map)
    //   - all-zero grid             → every cell at 0.0 (no data)
    //   - varied grid starting at 0 → same scale as the old min-max form
    //
    // Only an all-zero grid yields all-zero heat; that case is the "no
    // data" signal, not a uniform one.

    #[test]
    fn heat_map_uniform_non_zero_returns_all_ones() {
        // A panel that has uniformly accumulated the same on-hours in
        // every cell must NOT read as flat grey / zero heat — that would
        // be indistinguishable from a fresh ledger with no samples.
        let mut l = WearLedger::new(ident(), PanelType::QdOled, 1, 3, 0);
        l.cells[0].wear_hours = 2.0;
        l.cells[1].wear_hours = 2.0;
        l.cells[2].wear_hours = 2.0;
        assert_eq!(l.heat_map(), vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn heat_map_zero_max_matches_min_max_for_grids_starting_at_zero() {
        // When `min == 0`, zero-max and min-max produce the same
        // scale — this pins the contract that the fix is behavior-
        // preserving for already-visible (varied) grids.
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 3, 0);
        l.cells[0].wear_hours = 0.0;
        l.cells[1].wear_hours = 2.0;
        l.cells[2].wear_hours = 4.0;
        assert_eq!(l.heat_map(), vec![0.0, 0.5, 1.0]);
    }

    #[test]
    fn heat_map_all_zero_grid_returns_all_zero() {
        // A fresh / unsampled ledger is the genuine "no data" case and
        // must keep reading as zero heat, not as a uniform 1.0 panel.
        let l = WearLedger::new(ident(), PanelType::Unknown, 1, 2, 0);
        assert_eq!(l.heat_map(), vec![0.0, 0.0]);
    }

    #[test]
    fn brightness_norm_scales_and_falls_back() {
        let ddc = PanelState {
            power: None,
            brightness: Some(80),
        };
        assert!((brightness_norm(&ddc, 100, 0.5) - 0.8).abs() < 1e-9);
        let samsung = PanelState {
            power: None,
            brightness: Some(25),
        };
        assert!((brightness_norm(&samsung, 50, 0.5) - 0.5).abs() < 1e-9);
        let none = PanelState::default();
        assert!((brightness_norm(&none, 100, 0.42) - 0.42).abs() < 1e-9);
    }

    #[test]
    fn resize_grid_conserves_total() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 2, 2, 0);
        l.attribute_uniform(Duration::from_secs(7200), 1.0); // 2.0 total
        let before = l.total_on_hours;
        l.resize_grid(4, 4);
        assert_eq!(l.cells.len(), 16);
        assert!((l.total_on_hours - before).abs() < 1e-9);
        assert_eq!((l.grid_rows, l.grid_cols), (4, 4));
    }

    #[test]
    fn resize_grid_maps_spatial_density_not_flat_redistribution() {
        // P16 — pins the semantics, not just conservation
        // 1×2 grid with UNEVEN wear: left cell 4.0h, right cell 0.0h.
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 2, 0);
        l.cells[0].wear_hours = 4.0;
        l.total_on_hours = 4.0;
        // Upsample to 1×4: left half's density must land in the two left cells.
        l.resize_grid(1, 4);
        let w: Vec<f64> = l.cells.iter().map(|c| c.wear_hours).collect();
        assert!(
            (w[0] - 2.0).abs() < 1e-9 && (w[1] - 2.0).abs() < 1e-9,
            "left-half density must map to left cells, got {w:?}"
        );
        assert!(
            w[2].abs() < 1e-9 && w[3].abs() < 1e-9,
            "right half had zero wear — flat redistribution would put 1.0 in each cell"
        );
        // Downsample back to 1×2 restores the original split.
        l.resize_grid(1, 2);
        assert!((l.cells[0].wear_hours - 4.0).abs() < 1e-9 && l.cells[1].wear_hours.abs() < 1e-9);
    }

    #[test]
    fn resize_grid_to_zero_dimension_resets_total() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 2, 2, 0);
        l.attribute_uniform(Duration::from_secs(7200), 1.0); // 2.0 total
        l.resize_grid(0, 0);
        assert!(l.cells.is_empty());
        assert_eq!(l.total_on_hours, 0.0);
    }

    #[test]
    fn serde_round_trip_and_epoch_fields() {
        let mut l = WearLedger::new(ident(), PanelType::QdOled, 9, 16, 123);
        l.last_sample_at_epoch_s = Some(456);
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"qd-oled\""));
        let back: WearLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_sample_at_epoch_s, Some(456));
        assert_eq!(back.advisory_baseline_epoch_s, 123);
    }

    #[test]
    fn sanitize_identity_key_rules() {
        assert_eq!(
            sanitize_identity_key("ddc:AOC:AG326UZD:XK2R9JA000013"),
            "ddc-aoc-ag326uzd-xk2r9ja000013"
        );
        assert_eq!(
            sanitize_identity_key("samsung:192.0.2.10"),
            "samsung-192.0.2.10"
        );
        let long = "x".repeat(100);
        assert_eq!(sanitize_identity_key(&long).len(), 64);
    }

    #[test]
    fn future_schema_version_is_detectable() {
        let mut l = WearLedger::new(ident(), PanelType::Unknown, 1, 1, 0);
        l.schema_version = 99;
        let json = serde_json::to_string(&l).unwrap();
        let back: WearLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, 99); // loader (T7) branches on this — parsing itself succeeds
    }

    // ── W1 review fix: shared advisory formula ─────────────────────────────
    // Single-source-of-truth test for `advisory_active`/
    // `hours_since_effective_dwell` — both `dormantd::wear_tracker::tick`
    // and `dormant_web::routes::wear::summarize` call through these instead
    // of independently re-deriving the arithmetic, so this is the one test
    // that has to hold for both call sites to stay in sync.

    #[test]
    fn advisory_active_false_when_baseline_recent() {
        let now = 1_000_000;
        // Baseline 1h ago, advisory_after = 2h -> not yet advisory.
        assert!(!advisory_active(
            None,
            now - 3600,
            Duration::from_secs(7200),
            now
        ));
    }

    #[test]
    fn advisory_active_true_when_baseline_older_than_advisory_after() {
        let now = 1_000_000;
        // Baseline 2h ago, advisory_after = 1h -> advisory.
        assert!(advisory_active(
            None,
            now - 7200,
            Duration::from_secs(3600),
            now
        ));
    }

    #[test]
    fn advisory_active_uses_observed_dwell_when_more_recent_than_baseline() {
        let now = 1_000_000;
        let old_baseline = now - 10 * 3600;
        let recent_dwell = now - 30 * 60; // 30 min ago
        // Observed dwell wins over the much-older baseline -> not advisory
        // even though advisory_after is small.
        assert!(!advisory_active(
            Some(recent_dwell),
            old_baseline,
            Duration::from_secs(3600),
            now
        ));
    }

    #[test]
    fn hours_since_effective_dwell_matches_max_of_observed_and_baseline() {
        let now = 1_000_000;
        let old_baseline = now - 10 * 3600;
        let recent_dwell = now - 2 * 3600;
        assert_eq!(
            hours_since_effective_dwell(Some(recent_dwell), old_baseline, now),
            2,
            "a more-recent observed dwell must win over the older baseline"
        );
        assert_eq!(
            hours_since_effective_dwell(None, old_baseline, now),
            10,
            "with no observed dwell, hours are measured from the baseline"
        );
    }

    proptest::proptest! {
        #[test]
        fn resize_conserves_total_prop(r1 in 1u16..12, c1 in 1u16..12, r2 in 1u16..12, c2 in 1u16..12, hours in 0.0f64..1000.0) {
            let mut l = WearLedger::new(ident(), PanelType::Unknown, r1, c1, 0);
            l.attribute_uniform(Duration::from_secs_f64(hours * 3600.0), 1.0);
            let before = l.total_on_hours;
            l.resize_grid(r2, c2);
            proptest::prop_assert!((l.total_on_hours - before).abs() < 1e-6);
        }
    }
}
