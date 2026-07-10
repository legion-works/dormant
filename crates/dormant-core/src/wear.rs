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
    /// Total brightness-weighted on-hours across the whole panel.
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
        let n = brightness_norm.clamp(0.0, 1.0);
        let h = span.as_secs_f64() / 3600.0 * n;
        for c in &mut self.cells {
            c.wear_hours += h;
        }
        self.total_on_hours += h;
        self.sample_count += 1;
    }

    /// Min-max normalized wear per cell, in row-major order, each in
    /// `0.0..=1.0`.
    ///
    /// When every cell has (near-)equal wear (`max - min < f64::EPSILON`,
    /// including the all-zero starting grid), returns all zeros rather than
    /// dividing by a near-zero range — a flat grid has no "hottest" cell to
    /// normalize against.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "heat_map is a display-precision output (0.0..=1.0); f64->f32 narrowing here is intentional, not an accumulator"
    )]
    pub fn heat_map(&self) -> Vec<f32> {
        let min = self
            .cells
            .iter()
            .map(|c| c.wear_hours)
            .fold(f64::INFINITY, f64::min);
        let max = self
            .cells
            .iter()
            .map(|c| c.wear_hours)
            .fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < f64::EPSILON {
            return vec![0.0; self.cells.len()];
        }
        let range = max - min;
        self.cells
            .iter()
            .map(|c| ((c.wear_hours - min) / range) as f32)
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

/// Shared, lock-guarded map of wear ledgers keyed by config `DisplayId`
/// string, handed to whichever components (tracker, IPC handlers) need
/// concurrent read/write access to live ledgers.
pub type WearHandle = Arc<RwLock<HashMap<String, WearLedger>>>;

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
