//! Pure per-pixel crossfade blend — `out = capture*(1-t) + new*t`.
//!
//! Spike-measured (see `/tmp/opencode/p21-fade-spike/report.md` Q3):
//! plain safe-Rust u8 lerp at 3072×1728 is 0.9 ms/frame, ~1106 fps
//! — well under any reasonable frame budget.  No SIMD intrinsics;
//! the compiler auto-vectorizes the simple loop on `x86_64`.
//!
//! Allocation-free per call: the caller owns all three buffers (the
//! capture buffer is reused across transitions).  The arithmetic is
//! `u16` (the spike's `t_frac` scale) to keep the per-pixel lerp
//! branch-free in the hot loop.

/// Minimum `t_frac` value: 0 = full capture, never user-facing but kept
/// as a named const for tests / docs to anchor the canonical range.
/// (The blend loops use the inline literal `0`.)
#[allow(dead_code)] // named anchor for the t_frac scale; loops inline-literal `0` for clarity
pub const T_MIN: u16 = 0;
/// Sentinel value meaning "fully transitioned".  The caller compares
/// the live `t_frac` against this to decide when to drop the transition
/// timer (the state machine in `state.rs` clamps to this cap).
pub const T_MAX: u16 = 256;

/// Three-buffer blend: write `a*(1-t) + b*t` into `out`.
///
/// `t_frac` is `0..=256` — 0 keeps all of `a`, 256 keeps all of `b`,
/// intermediate values linearly interpolate per byte.  Values above
/// 256 are clamped to 256 (i.e. treated as "fully transitioned").
///
/// `a`, `b`, and `out` are all `&[u8]`/`&mut [u8]` — Rust's aliasing
/// rules prevent passing the same slice as two of them; the
/// `blend_in_place` variant below handles the screensaver's
/// rendered-into-back-buffer + capture case where one of the inputs
/// aliases the output.
///
/// # Panics
///
/// Panics if the three slices have different lengths; this is a bug
/// in the caller (the buffers all describe the same frame).
///
/// # Performance
///
/// On `x86_64` the compiler auto-vectorises the loop into SIMD; benchmark
/// at 3072×1728 (AOC): 0.9 ms / 1106 fps — see spike report `Q3`.
#[allow(dead_code)]
// public utility — `blend_in_place` is the screensaver hot path; three-buffer variant is exported for spike parity + tests
#[inline]
pub fn blend(a: &[u8], b: &[u8], out: &mut [u8], t_frac: u16) {
    assert_eq!(a.len(), b.len(), "blend: a/b length mismatch");
    assert_eq!(a.len(), out.len(), "blend: out length mismatch");

    let t = t_frac.min(T_MAX);
    let inv = T_MAX - t; // 0..=256

    for i in 0..a.len() {
        // Branch-free u8 lerp on u16 to dodge the u8 overflow:
        // ((a as u16) * inv + (b as u16) * t) >> 8 ∈ [0, 255].
        out[i] = (((u16::from(a[i])) * inv + (u16::from(b[i])) * t) >> 8) as u8;
    }
}

/// In-place blend: `*buf = capture*(1-t) + *buf*t`.
///
/// Specialised for the screensaver's hot loop where the caller wants
/// to mutate the freshly-rendered frame in place rather than copy into
/// a third buffer.  Both `capture` and `buf` have the same length; the
/// borrow checker can't prove these are disjoint for a plain
/// `&[u8]`/`&mut [u8]` pair, so this function takes the capture
/// from a `Vec<u8>` reference (the caller owns the buffer and the
/// aliasing is impossible — the captures live in `TransitionState`
/// and the buffers live in the SHM pool).
#[inline]
pub fn blend_in_place(capture: &[u8], buf: &mut [u8], t_frac: u16) {
    assert_eq!(capture.len(), buf.len(), "blend_in_place: length mismatch");

    let t = t_frac.min(T_MAX);
    let inv = T_MAX - t;

    for i in 0..capture.len() {
        buf[i] = (((u16::from(capture[i])) * inv + (u16::from(buf[i])) * t) >> 8) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a solid-colour buffer at runtime (16 bytes is enough
    /// to assert everything we care about without paying for a
    /// 4K-aligned zero-init in every test).
    fn solid(c: u8) -> Vec<u8> {
        vec![c; 16]
    }

    #[test]
    fn t_zero_preserves_a_entirely() {
        // t=0 must reproduce capture byte-for-byte — the very first
        // tick of the blend.
        let a = solid(0x10);
        let b = solid(0xF0);
        let mut out = vec![0u8; 16];
        blend(&a, &b, &mut out, T_MIN);
        assert_eq!(out, a, "t=0 must keep a intact");
    }

    #[test]
    fn t_max_uses_b_entirely() {
        // t=MAX (256) must reproduce the new frame byte-for-byte —
        // the last tick of the blend.
        let a = solid(0x10);
        let b = solid(0xF0);
        let mut out = vec![0u8; 16];
        blend(&a, &b, &mut out, T_MAX);
        assert_eq!(out, b, "t=256 must yield b intact");
    }

    #[test]
    fn t_clamps_above_max() {
        // A caller that nudges t past 256 (rounding error, off-by-one)
        // must still produce the b-side result instead of overflowing
        // into nonsense.  The clamp is the predicate for "transition
        // complete" so it must be exact.
        let a = solid(0x10);
        let b = solid(0xF0);
        let mut out = vec![0u8; 16];
        blend(&a, &b, &mut out, 257);
        assert_eq!(out, b, "t>256 must clamp to b");
        blend(&a, &b, &mut out, u16::MAX);
        assert_eq!(out, b, "t=u16::MAX must clamp to b");
    }

    #[test]
    fn midpoint_is_average_rounded_toward_b() {
        // At t=128 (50%) the blend must equal (a + b) / 2 (rounded).
        // The shift-by-8 is the rounding direction the spike committed
        // to; this test pins it so a future "shift by 7 for accuracy"
        // tweak can't sneak in.
        let a = vec![100u8; 16];
        let b = vec![200u8; 16];
        let mut out = vec![0u8; 16];
        blend(&a, &b, &mut out, 128);
        let expected = vec![150u8; 16]; // (100 + 200) / 2 = 150
        assert_eq!(out, expected, "midpoint must equal integer average");
    }

    #[test]
    fn monotonic_ramp_matches_linear_lerp() {
        // Sweep t from 0 to 256 and assert the output is monotonically
        // non-decreasing in the b direction (a low, b high) and
        // matches a hand-computed u8 lerp for every step.  This is the
        // primary regression guard against off-by-one rounding errors
        // and sign-extension bugs.
        let a = vec![10u8; 8];
        let b = vec![250u8; 8];
        let mut out = vec![0u8; 8];
        let mut last = a[0];
        for t in 0..=T_MAX {
            blend(&a, &b, &mut out, t);
            assert_eq!(
                out[0],
                ((u16::from(a[0]) * (T_MAX - t) + u16::from(b[0]) * t) >> 8) as u8,
                "t={t}: blend output must match direct u16 lerp formula"
            );
            // Monotonic towards b (since a < b).
            assert!(
                out[0] >= last,
                "t={t}: output went backwards ({last}→{})",
                out[0]
            );
            last = out[0];
        }
    }

    #[test]
    fn handles_odd_buffer_sizes() {
        // The raw buffer length isn't always a multiple of 4 or 16 —
        // mpv's stride is width*4 but the actual rendered slice is
        // height*stride bytes (which may overshoot by padding).  The
        // blend must accept any length without panicking on the tail.
        let a = vec![42u8; 17]; // odd length
        let b = vec![99u8; 17];
        let mut out = vec![0u8; 17];
        blend(&a, &b, &mut out, 128);
        for &v in &out {
            assert_eq!(v, 70, "odd-len midpoint must also be 50/50 rounded");
        }
    }

    #[test]
    fn empty_buffer_is_no_op() {
        // The buffer is empty when configured-size window hasn't
        // produced a first frame yet, OR when the compositor went
        // through a resize that reset the capture.  blend must be a
        // no-op (no panic, no write) on zero-length slices.
        let a: [u8; 0] = [];
        let b: [u8; 0] = [];
        let mut out: [u8; 0] = [];
        blend(&a, &b, &mut out, 128);
        assert_eq!(out.len(), 0);
    }

    #[test]
    #[should_panic(expected = "a/b length mismatch")]
    fn mismatched_a_b_lengths_panics() {
        // Two buffers of different lengths describe different frames;
        // this is a caller bug, not a runtime case.  Panic now (loud)
        // rather than rendering garbage.
        let a = vec![0u8; 4];
        let b = vec![0u8; 8];
        let mut out = vec![0u8; 8];
        blend(&a, &b, &mut out, 128);
    }

    #[test]
    #[should_panic(expected = "out length mismatch")]
    fn mismatched_out_length_panics() {
        // Same reasoning — caller bug.
        let a = vec![0u8; 4];
        let b = vec![0u8; 4];
        let mut out = vec![0u8; 5];
        blend(&a, &b, &mut out, 128);
    }

    // ── blend_in_place ─────────────────────────────────────────────────

    #[test]
    fn blend_in_place_t_zero_keeps_capture() {
        // t=0 must leave the buffer entirely as capture — the first
        // tick of an in-place blend (no new pixels have arrived yet
        // either; conceptually `buf == capture`).
        let capture = solid(0x10);
        let mut buf = solid(0x10);
        blend_in_place(&capture, &mut buf, T_MIN);
        assert_eq!(buf, capture, "t=0 in-place must preserve capture bytes");
    }

    #[test]
    fn blend_in_place_t_max_overwrites_with_buf() {
        // t=256 (max) leaves buf as it is — the caller already drew
        // the new frame into buf, so the result IS buf.
        let capture = solid(0x10);
        let mut buf = solid(0xF0);
        let before = buf.clone();
        blend_in_place(&capture, &mut buf, T_MAX);
        assert_eq!(buf, before, "t=MAX in-place must leave buf untouched");
    }

    #[test]
    fn blend_in_place_midpoint_matches_separate_blend() {
        // The two functions must produce identical results at the
        // midpoint — same arithmetic, just different aliasing rules.
        let cap = vec![100u8; 8];
        let mid_vec = vec![200u8; 8];
        let mut buf_in_place = mid_vec.clone();
        blend_in_place(&cap, &mut buf_in_place, 128);

        let mut buf_separate = vec![0u8; 8];
        blend(&cap, &mid_vec, &mut buf_separate, 128);
        assert_eq!(
            buf_in_place, buf_separate,
            "in-place midpoint must equal three-buffer midpoint"
        );
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn blend_in_place_panics_on_length_mismatch() {
        // Caller bug — surfacing as a panic is the right answer.
        let cap = vec![0u8; 4];
        let mut buf = vec![0u8; 8];
        blend_in_place(&cap, &mut buf, 128);
    }
}
