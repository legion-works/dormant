//! Pure pixel-shift raster-walk logic — no Wayland deps.
//!
//! Mechanism (decided by the live probe —
//! `docs/research/2026-07-11-pixel-shift-probe.md`): **`wp_viewport`
//! source-rect shift**.  The Wayland glue in `crate::linux` oversizes
//! the shm buffer by [`margin`] on each side, then walks the
//! viewport's source rect through the deterministic cycle computed by
//! [`raster_offsets`] on a timer — each step is `set_source` +
//! `damage_buffer` + `commit`, no re-render, no new attach.
//!
//! Everything in this module is pure data / arithmetic so it can be
//! unit-tested without a compositor.  [`ShiftState`] is the small
//! stateful cursor the Wayland glue drives; the free functions
//! ([`margin`], [`raster_offsets`]) are the math it's built on.

/// Per-side margin (in **buffer pixels**) the oversized buffer needs
/// for a given raster-walk step size.
///
/// Per the adjudicated margin/walk rule: `max_radius = 2 × shift_px`,
/// and the margin on EACH side of the buffer equals `max_radius` —
/// i.e. this same quantity serves both as "how far the walk can move
/// off-centre" and "how much bigger the buffer must be on each edge".
/// Matches the probe exactly (2px step ⇒ 4px max radius ⇒ margin
/// used was 8px = "2× headroom" quoted in the probe write-up, i.e.
/// `2 × max_radius` total extra size, `max_radius` per side).
///
/// `step_px = 0` returns `0` (shift disabled — the caller never
/// oversizes anything).
#[must_use]
pub fn margin(step_px: u8) -> u32 {
    2 * u32::from(step_px)
}

/// Deterministic raster-walk cycle over `±max_radius` in `step_px`
/// increments.
///
/// - `step_px == 0` → empty (shift disabled).
/// - Every offset's `x` and `y` is an exact multiple of `step_px`
///   (built from `k * step_px` on each axis, not by naively slicing
///   `-radius..=radius`, so this holds even when `max_radius` isn't a
///   clean multiple of `step_px`).
/// - `|x| <= max_radius` and `|y| <= max_radius` for every offset.
/// - The cycle starts AND ends at `(0, 0)` — the walk always returns
///   to dead-centre, and [`ShiftState::advance`] wrapping from the
///   last element back to the first is itself a (harmless) origin →
///   origin step.
#[must_use]
pub fn raster_offsets(step_px: u8, max_radius: u8) -> Vec<(i32, i32)> {
    if step_px == 0 {
        return Vec::new();
    }
    let step = i32::from(step_px);
    let radius = i32::from(max_radius);

    // Symmetric multiples of `step` within `[-radius, radius]`,
    // ascending.  Always contains 0.
    let mut axis = Vec::new();
    let mut k: i32 = 0;
    loop {
        let val = k * step;
        if val > radius {
            break;
        }
        if val == 0 {
            axis.push(0);
        } else {
            axis.push(-val);
            axis.push(val);
        }
        k += 1;
    }
    axis.sort_unstable();

    // Raster (row-major, boustrophedon) walk over the axis × axis
    // grid so consecutive steps are always spatially adjacent.
    let mut offsets = Vec::with_capacity(axis.len() * axis.len() + 1);
    for (row, &y) in axis.iter().enumerate() {
        if row % 2 == 0 {
            for &x in &axis {
                offsets.push((x, y));
            }
        } else {
            for &x in axis.iter().rev() {
                offsets.push((x, y));
            }
        }
    }

    // Rotate so the cycle STARTS at the origin (visually: a freshly
    // shown surface is centred before the first timer tick ever
    // fires) — `axis` always contains 0, so `(0, 0)` is always in
    // `offsets`.
    if let Some(origin_idx) = offsets.iter().position(|&o| o == (0, 0)) {
        offsets.rotate_left(origin_idx);
    }

    // ...and ENDS at the origin too, closing the loop explicitly.
    offsets.push((0, 0));

    offsets
}

/// Stateful cursor over a [`raster_offsets`] cycle, plus the margin
/// math and the walk-offset → source-rect-origin mapping in one
/// place — the Wayland glue only ever calls [`ShiftState::new`],
/// [`ShiftState::source_origin`], and [`ShiftState::advance`].
#[derive(Debug, Clone)]
pub struct ShiftState {
    margin_px: u32,
    offsets: Vec<(i32, i32)>,
    cursor: usize,
}

impl ShiftState {
    /// Build the walk state for a given `step_px` (buffer pixels —
    /// see the config-consuming field doc on
    /// `crate::settings::ShiftSettings::shift_px`).  `step_px == 0`
    /// yields a disabled state (`enabled()` is `false`, `advance`ing
    /// is a no-op that stays at dead-centre).
    #[must_use]
    pub fn new(step_px: u8) -> Self {
        let margin_px = margin(step_px);
        // `margin_px = 2 * step_px` and `step_px <= 8` (validator
        // ceiling) always fits in u8 (<= 16); the fallback is
        // defensive only.
        let max_radius = u8::try_from(margin_px).unwrap_or(u8::MAX);
        Self {
            margin_px,
            offsets: raster_offsets(step_px, max_radius),
            cursor: 0,
        }
    }

    /// `true` when this state carries a non-empty walk cycle (i.e.
    /// `step_px > 0` was passed to [`Self::new`]).
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.offsets.is_empty()
    }

    /// Per-side buffer margin this state was built with.
    #[must_use]
    pub fn margin_px(&self) -> u32 {
        self.margin_px
    }

    /// Source-rect origin `(x, y)` for the walk's CURRENT position,
    /// within the oversized buffer.  `(margin, margin)` (dead-centre)
    /// when disabled or at the cycle's origin entry.
    #[must_use]
    pub fn source_origin(&self) -> (u32, u32) {
        let (ox, oy) = self.offsets.get(self.cursor).copied().unwrap_or((0, 0));
        Self::offset_to_origin(ox, oy, self.margin_px)
    }

    /// Advance to the next offset in the cycle (wraps past the end
    /// back to the start).  Returns the new [`Self::source_origin`].
    /// No-op (stays at dead-centre) when disabled.
    pub fn advance(&mut self) -> (u32, u32) {
        if self.offsets.is_empty() {
            return (self.margin_px, self.margin_px);
        }
        self.cursor = (self.cursor + 1) % self.offsets.len();
        self.source_origin()
    }

    /// Pure mapping: a (possibly negative) walk offset from centre,
    /// plus the per-side margin, to a `wp_viewport::set_source`
    /// origin.  Always within `0..=2*margin_px` — i.e. never negative
    /// and never exceeds the margin budget the oversized buffer was
    /// built with.  Clamped defensively; [`raster_offsets`] already
    /// guarantees offsets never exceed `max_radius`, but a caller
    /// that passes a `margin_px` smaller than the offset's own
    /// `max_radius` (a construction bug, not a runtime state) is
    /// still made safe here rather than panicking or wrapping.
    #[must_use]
    pub fn offset_to_origin(offset_x: i32, offset_y: i32, margin_px: u32) -> (u32, u32) {
        let budget = i64::from(margin_px);
        let x = (budget + i64::from(offset_x)).clamp(0, budget * 2);
        let y = (budget + i64::from(offset_y)).clamp(0, budget * 2);
        // Clamp above guarantees the range 0..=2*budget; `try_from`
        // only fails for pathologically large `margin_px` (near
        // `u32::MAX`, unreachable given `shift_px <= 8` in practice) —
        // the fallback keeps this total rather than panicking.
        let x = u32::try_from(x).unwrap_or(u32::MAX);
        let y = u32::try_from(y).unwrap_or(u32::MAX);
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── margin ───────────────────────────────────────────────────────

    #[test]
    fn margin_is_double_step_px() {
        assert_eq!(margin(0), 0);
        assert_eq!(margin(1), 2);
        assert_eq!(margin(2), 4);
        assert_eq!(margin(8), 16);
    }

    // ── raster_offsets ───────────────────────────────────────────────

    #[test]
    fn raster_offsets_step_zero_is_empty() {
        assert!(raster_offsets(0, 8).is_empty());
        assert!(raster_offsets(0, 0).is_empty());
    }

    #[test]
    fn raster_offsets_starts_and_ends_at_origin() {
        let offsets = raster_offsets(2, 4);
        assert_eq!(offsets.first(), Some(&(0, 0)), "cycle must start centred");
        assert_eq!(offsets.last(), Some(&(0, 0)), "cycle must return to centre");
    }

    #[test]
    fn raster_offsets_bounded_by_max_radius() {
        let max_radius = 4;
        for &(x, y) in &raster_offsets(2, max_radius) {
            assert!(
                x.abs() <= i32::from(max_radius),
                "x={x} exceeds max_radius={max_radius}"
            );
            assert!(
                y.abs() <= i32::from(max_radius),
                "y={y} exceeds max_radius={max_radius}"
            );
        }
    }

    #[test]
    fn raster_offsets_all_multiples_of_step() {
        let step = 3;
        // Deliberately NOT a clean multiple of step, to prove the
        // "multiple of step" guarantee holds even then.
        let max_radius = 8;
        for &(x, y) in &raster_offsets(step, max_radius) {
            assert_eq!(x % i32::from(step), 0, "x={x} not a multiple of {step}");
            assert_eq!(y % i32::from(step), 0, "y={y} not a multiple of {step}");
        }
    }

    #[test]
    fn raster_offsets_covers_full_grid_including_extremes() {
        // step=2, radius=4 (the probe's own fixture: shift_px=2,
        // max_radius=2*2=4) must visit every corner of the walked
        // square at least once.
        let offsets = raster_offsets(2, 4);
        for extreme in [(-4, -4), (4, -4), (-4, 4), (4, 4), (0, 0)] {
            assert!(
                offsets.contains(&extreme),
                "raster walk must visit {extreme:?}, got {offsets:?}"
            );
        }
    }

    #[test]
    fn raster_offsets_default_shift_px_matches_probe_fixture() {
        // Adjudicated rule: max_radius = 2 * shift_px. Default
        // shift_px (dormant_core::config::defaults::SHIFT_PX) is 2,
        // so max_radius = 4 — exactly the probe's own "2px steps
        // within ±4" description.
        let step_px = 2;
        let max_radius = 2 * step_px;
        let offsets = raster_offsets(step_px, max_radius);
        assert!(offsets.iter().all(|&(x, y)| x.abs() <= 4 && y.abs() <= 4));
    }

    // ── ShiftState ───────────────────────────────────────────────────

    #[test]
    fn shift_state_disabled_when_step_zero() {
        let s = ShiftState::new(0);
        assert!(!s.enabled());
        assert_eq!(s.margin_px(), 0);
        assert_eq!(s.source_origin(), (0, 0));
        let mut s = s;
        assert_eq!(
            s.advance(),
            (0, 0),
            "advance on a disabled state is a no-op"
        );
    }

    #[test]
    fn shift_state_enabled_when_step_nonzero() {
        let s = ShiftState::new(2);
        assert!(s.enabled());
        assert_eq!(s.margin_px(), 4);
    }

    #[test]
    fn shift_state_starts_centred() {
        let s = ShiftState::new(2);
        // margin=4, centre offset (0,0) -> origin (4,4).
        assert_eq!(s.source_origin(), (4, 4));
    }

    #[test]
    fn shift_state_wraps_cleanly() {
        let mut s = ShiftState::new(2);
        let max_radius = u8::try_from(margin(2)).expect("margin(2)=4 fits u8");
        let cycle_len = raster_offsets(2, max_radius).len();
        // Advance exactly `cycle_len` times: since offsets[0] and
        // offsets[cycle_len - 1] are BOTH the origin (start/end rule),
        // advancing cycle_len times lands back on offsets[0] again —
        // i.e. two consecutive centre visits at the wrap, then the
        // walk resumes identically to a fresh state.
        let mut last = s.source_origin();
        for _ in 0..cycle_len {
            last = s.advance();
        }
        assert_eq!(
            last,
            s.source_origin(),
            "state after cycle_len advances must be self-consistent"
        );
        // One more full lap must reproduce the exact same sequence
        // (deterministic wrap, no drift).
        let mut first_lap = Vec::new();
        let mut s2 = ShiftState::new(2);
        for _ in 0..cycle_len {
            first_lap.push(s2.advance());
        }
        let mut second_lap = Vec::new();
        for _ in 0..cycle_len {
            second_lap.push(s2.advance());
        }
        assert_eq!(first_lap, second_lap, "the walk must repeat identically");
    }

    #[test]
    fn shift_state_never_exceeds_margin_budget() {
        let mut s = ShiftState::new(2);
        let margin = s.margin_px();
        for _ in 0..64 {
            let (x, y) = s.advance();
            assert!(x <= 2 * margin, "x={x} exceeds 2*margin={}", 2 * margin);
            assert!(y <= 2 * margin, "y={y} exceeds 2*margin={}", 2 * margin);
        }
    }

    // ── offset_to_origin ─────────────────────────────────────────────

    #[test]
    fn offset_to_origin_never_negative_never_exceeds_budget() {
        let margin_px = 8;
        for offset in [-8, -4, -1, 0, 1, 4, 8] {
            let (x, y) = ShiftState::offset_to_origin(offset, offset, margin_px);
            assert!(x <= 2 * margin_px, "x={x} out of budget");
            assert!(y <= 2 * margin_px, "y={y} out of budget");
        }
    }

    #[test]
    fn offset_to_origin_clamps_out_of_range_defensively() {
        // Construction bug (offset larger than the margin it's paired
        // with) must clamp, not panic or wrap.
        let (x, y) = ShiftState::offset_to_origin(-100, 100, 4);
        assert_eq!(x, 0);
        assert_eq!(y, 8);
    }

    #[test]
    fn offset_to_origin_zero_offset_is_dead_centre() {
        assert_eq!(ShiftState::offset_to_origin(0, 0, 4), (4, 4));
        assert_eq!(ShiftState::offset_to_origin(0, 0, 0), (0, 0));
    }
}
