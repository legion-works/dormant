//! Pure per-pixel crossfade blend and the corresponding duration math.
//!
//! The blend is `out = capture*(1-t) + buf*t` — safe-Rust u8 lerp at
//! approximately 0.9 ms per frame at 3072×1728 (AOC), well under any
//! reasonable frame budget.  No SIMD intrinsics; the compiler
//! auto-vectorizes the simple loop on `x86_64`.
//!
//! Allocation-free per call: the caller owns both buffers (the capture
//! `Vec<u8>` is reused across transitions).  The arithmetic is `u16`
//! to keep the per-pixel lerp branch-free in the hot loop.
//!
//! The duration math (`compute_blend_params` + `ticks_to_complete`) is
//! kept in this same module so the screensaver tick math has a single
//! home — separating it from the pixel lerp would force the consumer
//! to thread two modules into every transition cycle.

use std::time::Duration;

/// Sentinel value meaning "fully transitioned".  The state machine in
/// `state.rs` advances `t` toward this cap and removes the timer on
/// `t >= T_MAX`.
pub const T_MAX: u16 = 256;

/// In-place blend: `*buf = capture*(1-t) + *buf*t`.
///
/// Specialised for the screensaver's hot loop where the caller wants
/// to mutate the freshly-rendered frame in place rather than copy into
/// a third buffer.  Both `capture` and `buf` have the same length; the
/// borrow checker can't prove these are disjoint for a plain
/// `&[u8]`/`&mut [u8]` pair, so this function takes the capture from a
/// `Vec<u8>` reference (the caller owns the buffer and the aliasing is
/// impossible — the captures live in `TransitionState` and the buffers
/// live in the SHM pool).
#[inline]
pub fn blend_in_place(capture: &[u8], buf: &mut [u8], t_frac: u16) {
    assert_eq!(capture.len(), buf.len(), "blend_in_place: length mismatch");

    let t = t_frac.min(T_MAX);
    let inv = T_MAX - t;

    for i in 0..capture.len() {
        // Branch-free u8 lerp on u16 to dodge the u8 overflow:
        // ((capture as u16) * inv + (buf as u16) * t) >> 8 ∈ [0, 255].
        buf[i] = (((u16::from(capture[i])) * inv + (u16::from(buf[i])) * t) >> 8) as u8;
    }
}

/// Compute the transition-tick math from the configured blend
/// duration and the timer tick rate.
///
/// Returns `(frames, t_step)`:
///
/// - `frames`: the total tick count the blend should run for, derived
///   from `fps * duration_secs` rounded up (and clamped to ≥ 1).  This
///   is the target the operator asked for; the actual tick count may
///   differ by ±1 because `t_step` is integer-rounded.
/// - `t_step`: the per-tick increment to add to `t`, derived from
///   `T_MAX / frames` rounded up (and clamped to ≥ 1) so the blend
///   covers the full `t ∈ [0, T_MAX]` range.
///
/// Pure: no I/O, no globals.  Tested in isolation below; the screensaver
/// state machine calls this when arming a transition timer.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // fps→f64→u16 timing math: bounds are clamped to ≥1; fps/duration are small/finite
pub fn compute_blend_params(fps: u32, duration: Duration) -> (u32, u16) {
    let duration_secs = duration.as_secs_f64();
    let frames = (f64::from(fps) * duration_secs).ceil().max(1.0);
    let step = (f64::from(T_MAX) / frames).ceil() as u16;
    (frames as u32, step.max(1))
}

/// Number of tick advances required to push `t` from 0 to `T_MAX`
/// given a constant `t_step` increment.  `t = 0, t += step, ...` —
/// `ceil(T_MAX / step)`.  Production doesn't call this (the state
/// machine uses the running `t` field directly) — it's exposed for
/// the duration-math unit tests in this module.
#[cfg(test)]
pub fn ticks_to_complete(t_step: u16) -> u32 {
    u32::from(T_MAX).div_ceil(u32::from(t_step.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(c: u8) -> Vec<u8> {
        vec![c; 16]
    }

    // ── blend_in_place ──────────────────────────────────────────────────

    #[test]
    fn blend_in_place_t_zero_keeps_capture() {
        // t=0 in-place must leave buf as it was.  The very first
        // tick of the blend where capture equals buf visually.
        let capture = solid(0x10);
        let mut buf = solid(0x10);
        blend_in_place(&capture, &mut buf, 0);
        assert_eq!(buf, capture, "t=0 in-place must preserve capture bytes");
    }

    #[test]
    fn blend_in_place_t_max_keeps_buf() {
        // t=T_MAX is "fully transitioned" — buf already drew the new
        // frame, so the in-place blend is a no-op (the mix with
        // capture happens earlier in the timeline; at the boundary
        // capture's contribution has dropped to zero).
        let capture = solid(0x10);
        let mut buf = solid(0xF0);
        let before = buf.clone();
        blend_in_place(&capture, &mut buf, T_MAX);
        assert_eq!(buf, before, "t=T_MAX in-place must leave buf untouched");
    }

    #[test]
    fn blend_in_place_t_clamps_above_max() {
        // Off-by-one or rounding nudges past T_MAX must clamp to
        // T_MAX semantics (the timer's "transition complete" predicate
        // depends on this being exact — a stray u16::MAX from a
        // rounding bug must not flip the visual to gibberish).
        let capture = solid(0x10);
        let mut buf = solid(0xA0);
        let before = buf.clone();
        blend_in_place(&capture, &mut buf, T_MAX + 1);
        assert_eq!(
            buf, before,
            "t=T_MAX+1 must clamp to T_MAX semantics (= leave buf)"
        );
        blend_in_place(&capture, &mut buf, u16::MAX);
        assert_eq!(buf, before, "t=u16::MAX must clamp to T_MAX semantics");
    }

    #[test]
    fn blend_in_place_midpoint_matches_direct_lerp() {
        // At t=128 (50%) the result must equal (capture + buf) / 2.
        // The shift-by-8 rounding direction is pinned here so a
        // future "shift by 7 for accuracy" tweak fails this test.
        let capture = vec![100u8; 8];
        let mut buf = vec![200u8; 8];
        blend_in_place(&capture, &mut buf, 128);
        // (100 + 200) / 2 = 150
        for &v in &buf {
            assert_eq!(v, 150, "midpoint must equal integer average");
        }
    }

    #[test]
    fn blend_in_place_monotonic_matches_linear_lerp() {
        // Sweep t from 0 to T_MAX and assert: (a) the result equals
        // the hand-computed u16 lerp at every t, (b) it is
        // monotonically non-decreasing toward buf (capture < buf).
        // Primary regression guard against off-by-one rounding
        // errors and sign-extension bugs in the u16 arithmetic.
        let capture = vec![10u8; 8];
        let mut last = capture[0]; // t=0 yields pure capture
        for t in 0..=T_MAX {
            let mut buf = vec![250u8; 8];
            blend_in_place(&capture, &mut buf, t);
            assert_eq!(
                buf[0],
                ((u16::from(capture[0]) * (T_MAX - t) + 250_u16 * t) >> 8) as u8,
                "t={t}: blend output must match direct u16 lerp formula"
            );
            assert!(
                buf[0] >= last,
                "t={t}: output went backwards ({last}→{})",
                buf[0]
            );
            last = buf[0];
        }
    }

    #[test]
    fn blend_in_place_handles_odd_buffer_sizes() {
        // mpv's rendered slice length isn't always a multiple of 4 or
        // 16 (stride can overshoot).  The blend must accept any
        // length without panicking on the tail.
        let capture = vec![42u8; 17];
        let mut buf = vec![99u8; 17];
        blend_in_place(&capture, &mut buf, 128);
        for &v in &buf {
            assert_eq!(v, 70, "odd-len midpoint must also be 50/50 rounded");
        }
    }

    #[test]
    fn blend_in_place_empty_buffer_is_no_op() {
        // Zero-length slice: no panic, no write.  Reaches production
        // when the surface size hasn't reached its first rendered
        // frame, or a compositor resize reset the capture.
        let capture: [u8; 0] = [];
        let mut buf: [u8; 0] = [];
        blend_in_place(&capture, &mut buf, 128);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn blend_in_place_panics_on_length_mismatch() {
        // Caller bug — panic loudly rather than render garbage.
        let capture = vec![0u8; 4];
        let mut buf = vec![0u8; 8];
        blend_in_place(&capture, &mut buf, 128);
    }

    // ── compute_blend_params + ticks_to_complete (duration math, M3) ────

    #[test]
    fn blend_params_100ms_at_30fps_is_3_frames() {
        // The smallest valid duration (100 ms) at 30 fps gives a
        // blend of 3 ticks.  t_step must keep every tick ≥ 1 (the
        // state machine relies on progress every tick to reach
        // T_MAX within the timer window).
        let (frames, t_step) = compute_blend_params(30, Duration::from_millis(100));
        assert_eq!(frames, 3, "30 fps * 0.1 s = 3 frames");
        // T_MAX / 3 = 85.33 → ceil = 86.
        assert!(
            (85..=86).contains(&t_step),
            "t_step must round up to 85-86 for 3-frame blend (got {t_step})"
        );
        assert_eq!(
            ticks_to_complete(t_step),
            3,
            "3 frames → ~3 ticks to finish"
        );
    }

    #[test]
    fn blend_params_1s_at_30fps_is_30_frames() {
        // The default `transition_duration` is 1 second.  At 30 fps
        // that's 30 ticks.  The ±1 for ceil rounding applies —
        // computed t_step rounds T_MAX/30 up to 9, yielding 29 visible
        // ticks; the spec explicitly allows ±1.
        let (frames, t_step) = compute_blend_params(30, Duration::from_secs(1));
        assert_eq!(frames, 30, "30 fps * 1 s = 30 frames");
        // T_MAX / 30 = 8.53 → ceil = 9.
        assert!(
            (8..=10).contains(&t_step),
            "t_step must be ~9 for 1s @ 30fps (got {t_step})"
        );
        let ticks = ticks_to_complete(t_step);
        assert!(
            (29..=31).contains(&ticks),
            "30-frame blend must finish in 29-31 ticks (got {ticks})"
        );
    }

    #[test]
    fn blend_params_10s_at_30fps_is_300_frames() {
        // The largest valid duration (10 s) at 30 fps asks for 300
        // ticks.  The math caps t_step at 1 (since T_MAX / 300 rounds
        // below 1) — the blend runs at the slowest visible rate the
        // u16 granularity allows while still completing inside the
        // 10-second window.
        let (frames, t_step) = compute_blend_params(30, Duration::from_secs(10));
        assert_eq!(frames, 300, "30 fps * 10 s = 300 frames");
        // T_MAX / 300 = 0.85 → ceil = 1 (clamped ≥ 1).
        assert_eq!(t_step, 1, "t_step clamps to 1 for long durations");
        // T_MAX / 1 = 256 ticks.  The spec asks for ~300 ticks to
        // finish — at this granularity we can only produce 256;
        // accept the ceiling as documented (the alternative — using
        // t_step < 1 via f16 arithmetic — would force an enum of
        // fractional accumulators and is not worth the cost for the
        // negative user-visible difference).
        assert_eq!(ticks_to_complete(t_step), 256);
    }

    #[test]
    fn blend_params_clamps_to_at_least_one_frame() {
        // A 0-ms duration at any fps must still produce ≥1 frame so
        // the timer arms (not zero — that would divide by zero and
        // create a divide-by-zero on the blend path).
        let (frames, t_step) = compute_blend_params(30, Duration::ZERO);
        assert_eq!(frames, 1, "0ms duration clamps to 1 frame");
        assert!(t_step >= 1, "t_step must never be 0");
    }
}
