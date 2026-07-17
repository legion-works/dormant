//! macOS idle-clock doctor probe — two bounded raw readings from the idle
//! clock (`CGEventSourceSecondsSinceLastEventType` in production), used to
//! diagnose whether the clock itself is alive.
//!
//! This is deliberately NOT the same defense as `dormantd`'s
//! `MacosIdleGuard` (frozen-value detection across many polls, sanity caps,
//! startup grace, etc. — see `crates/dormantd/src/macos_idle.rs`). That
//! state machine lives in the daemon and drives real inhibition decisions
//! over time. This probe is a much smaller, one-shot doctor diagnostic:
//! read the clock twice, [`SAMPLE_INTERVAL`] apart, and report whether the
//! two raw readings differ. Two bit-identical raw readings back-to-back is
//! itself suspicious (real wall-clock idle time should have advanced by at
//! least `SAMPLE_INTERVAL` between the two reads) — that is this probe's
//! `Fail` signal. It never synthesizes input and never inhibits anything;
//! it only reports what the raw clock said.
//!
//! ## FFI duplication note
//!
//! `crates/dormantd/src/macos_idle.rs` already declares the real
//! `CGEventSourceSecondsSinceLastEventType` extern for its own (much
//! richer) idle-guard logic. This module cannot reuse that declaration:
//! `dormant-doctor` sits BELOW `dormantd` in the dependency graph
//! (`dormantd` depends on `dormant-doctor`, not the other way around — see
//! `crates/dormantd/Cargo.toml`), so reaching into `dormantd` from here
//! would be a cycle. The extern declaration below is therefore
//! intentionally duplicated (kept exactly as small as the one in
//! `dormantd`), and — like every other macOS FFI surface landed on this
//! branch — is DEFERRED: PR CI. It cannot compile or run in the Linux
//! sandbox this task was implemented in, and must be exercised for the
//! first time on the macOS CI lane (Task 2) or real hardware before being
//! trusted. The platform-neutral diagnosis logic above it (everything but
//! the `real` submodule) is fully exercised here on Linux.

// The platform-neutral diagnosis logic below (`MacosClock`,
// `probe_macos_idle_with`, `SAMPLE_INTERVAL`) is only ever reached in
// production from the `#[cfg(target_os = "macos")]`-gated `probe_macos_idle`
// at the bottom of this file — on a non-macOS, non-test build it is
// genuinely unreachable. Mirrors the identical situation (and identical
// fix) in `dormantd::macos_idle`'s own `macos_run`.
#![cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]

use std::time::Duration;

use crate::types::ProbeResult;

/// Bounded gap between the probe's two raw clock readings.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Injectable seam over "read seconds since any input event" so this
/// probe's diagnosis logic is testable without the real `CoreGraphics`
/// call. The real, macOS-only implementation is [`RealMacosClock`] below.
pub trait MacosClock: Send + Sync {
    /// Read the current idle duration, in seconds.
    ///
    /// # Errors
    ///
    /// Returns an error string when the underlying read fails. The real
    /// `CoreGraphics`-backed implementation never fails (the C API has no
    /// error path of its own); this exists so tests can exercise the
    /// failure path too.
    fn read(&self) -> Result<f64, String>;
}

/// Probe the macOS idle clock: two bounded raw readings, [`SAMPLE_INTERVAL`]
/// apart. Diagnoses `Fail` when the two readings are bit-identical (the
/// clock did not advance across a real wall-clock interval — a frozen or
/// unavailable clock); `Pass` otherwise. Never synthesizes input, never
/// blanks/wakes anything — purely a read-only diagnostic.
pub async fn probe_macos_idle_with(clock: &impl MacosClock) -> ProbeResult {
    let first = match clock.read() {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult::fail("macos-idle", format!("failed to read idle clock: {e}"));
        }
    };

    tokio::time::sleep(SAMPLE_INTERVAL).await;

    let second = match clock.read() {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult::fail("macos-idle", format!("failed to read idle clock: {e}"));
        }
    };

    if first.to_bits() == second.to_bits() {
        ProbeResult::fail(
            "macos-idle",
            format!(
                "idle clock returned two identical consecutive raw values: {first}, {second} \
                 — expected the value to advance across a {SAMPLE_INTERVAL:?} interval; the \
                 clock may be frozen or unavailable"
            ),
        )
    } else {
        ProbeResult::pass(
            "macos-idle",
            format!("idle clock advanced across the sample interval: {first}, {second}"),
        )
    }
}

// ── Real backend (macOS only) ────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod real {
    use super::MacosClock;

    /// `kCGEventSourceStateCombinedSessionState` — combines HID + private
    /// session event sources (mirrors `dormantd::macos_idle`'s constant).
    const K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE: i32 = 0;
    /// `kCGAnyInputEventType` — matches any input event type.
    const K_CG_ANY_INPUT_EVENT_TYPE: u32 = !0u32;

    #[allow(non_snake_case)]
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(stateID: i32, eventType: u32) -> f64;
    }

    /// Real `CoreGraphics`-backed clock. See the module docs' "FFI
    /// duplication note" for why this is a separate declaration from
    /// `dormantd::macos_idle`'s own copy.
    pub struct RealMacosClock;

    impl MacosClock for RealMacosClock {
        fn read(&self) -> Result<f64, String> {
            // Safety: both arguments are fixed constants matching Apple's
            // documented `CGEventSourceSecondsSinceLastEventType` signature;
            // the call has no error path (always returns a `double`).
            let secs = unsafe {
                CGEventSourceSecondsSinceLastEventType(
                    K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE,
                    K_CG_ANY_INPUT_EVENT_TYPE,
                )
            };
            Ok(secs)
        }
    }
}

#[cfg(target_os = "macos")]
pub use real::RealMacosClock;

/// Probe the real macOS idle clock. Only available on macOS.
#[cfg(target_os = "macos")]
pub async fn probe_macos_idle() -> ProbeResult {
    probe_macos_idle_with(&RealMacosClock).await
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;
    use std::sync::Mutex;

    /// Scripted clock: returns values from a fixed sequence, FIFO,
    /// repeating the last value once exhausted.
    struct ScriptedMacosClock {
        values: Vec<f64>,
        idx: Mutex<usize>,
    }

    impl ScriptedMacosClock {
        fn new(values: Vec<f64>) -> Self {
            Self {
                values,
                idx: Mutex::new(0),
            }
        }
    }

    impl MacosClock for ScriptedMacosClock {
        fn read(&self) -> Result<f64, String> {
            let mut idx = self.idx.lock().unwrap();
            let v = self.values[(*idx).min(self.values.len() - 1)];
            *idx += 1;
            Ok(v)
        }
    }

    struct FailingMacosClock;
    impl MacosClock for FailingMacosClock {
        fn read(&self) -> Result<f64, String> {
            Err("simulated read failure".to_string())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn macos_idle_probe_surfaces_consecutive_raw_values() {
        let clock = ScriptedMacosClock::new(vec![12.25, 12.25]);
        let result = probe_macos_idle_with(&clock).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(
            result.detail.contains("12.25, 12.25"),
            "detail should carry both raw values: {}",
            result.detail
        );
        assert!(
            result.detail.contains("identical"),
            "detail should say identical: {}",
            result.detail
        );
    }

    #[tokio::test(start_paused = true)]
    async fn macos_idle_probe_passes_when_clock_advances() {
        let clock = ScriptedMacosClock::new(vec![1.0, 1.5]);
        let result = probe_macos_idle_with(&clock).await;
        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(result.detail.contains("1, 1.5"), "{}", result.detail);
    }

    #[tokio::test(start_paused = true)]
    async fn macos_idle_probe_fails_on_read_error() {
        let clock = FailingMacosClock;
        let result = probe_macos_idle_with(&clock).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("simulated read failure"));
    }
}
