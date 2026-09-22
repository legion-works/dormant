//! Windows idle-clock doctor probe — two bounded raw readings from the idle
//! clock (`GetLastInputInfo` in production), used to diagnose whether the
//! clock itself is alive.
//!
//! Deliberately NOT the same defense as `dormantd`'s idle guard (frozen-value
//! detection across many polls, sanity caps, startup grace — see
//! `crates/dormantd/src/windows_idle.rs`). That state machine lives in the
//! daemon and drives real inhibition decisions over time. This probe is a
//! much smaller, one-shot doctor diagnostic: read the clock twice,
//! [`SAMPLE_INTERVAL`] apart, and report whether the two raw readings differ.
//! Two identical raw readings back-to-back is itself suspicious (real
//! wall-clock idle time should have advanced by at least `SAMPLE_INTERVAL`
//! between the two reads) — that is this probe's `Fail` signal. It never
//! synthesizes input and never inhibits anything; it only reports what the
//! raw clock said.
//!
//! ## FFI duplication note
//!
//! `crates/dormantd/src/windows_idle.rs` already declares the real
//! `GetLastInputInfo`/`GetTickCount` calls for its own (much richer) idle
//! logic. This module cannot reuse them: `dormant-doctor` sits BELOW
//! `dormantd` in the dependency graph (`dormantd` depends on `dormant-doctor`,
//! not the other way around), so reaching into `dormantd` from here would be a
//! cycle. The declarations below are therefore intentionally duplicated (kept
//! exactly as small as the ones in `dormantd`), and — like every other Windows
//! FFI surface on this branch — are DEFERRED: they cannot compile or run in
//! the Linux sandbox this was implemented in, and must be exercised for the
//! first time on the Windows CI lane or real hardware before being trusted.
//! The platform-neutral diagnosis logic above them (everything but the `real`
//! submodule) is fully exercised here on Linux.

// The platform-neutral diagnosis logic below (`WindowsClock`,
// `probe_windows_idle_with`, `SAMPLE_INTERVAL`) is only ever reached in
// production from the `#[cfg(target_os = "windows")]`-gated
// `probe_windows_idle` at the bottom of this file — on a non-Windows,
// non-test build it is genuinely unreachable. Mirrors the identical situation
// (and identical fix) in `dormantd::windows_idle`'s own `windows_run`.
#![cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]

use std::time::Duration;

use crate::types::ProbeResult;

/// Bounded gap between the probe's two raw clock readings.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Injectable seam over "read milliseconds since any input event" so this
/// probe's diagnosis logic is testable without the real `GetLastInputInfo`
/// call. The real, Windows-only implementation is [`RealWindowsClock`] below.
pub trait WindowsClock: Send + Sync {
    /// Read the current idle duration, in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns an error string when the underlying read fails. The real
    /// `GetLastInputInfo`-backed implementation returns this when the API
    /// reports failure; this exists so tests can exercise the failure path.
    fn read(&self) -> Result<u32, String>;
}

/// Probe the Windows idle clock: two bounded raw readings, [`SAMPLE_INTERVAL`]
/// apart. Diagnoses `Fail` when the two readings are identical (the clock did
/// not advance across a real wall-clock interval — a frozen or unavailable
/// clock); `Pass` otherwise. Never synthesizes input, never blanks/wakes
/// anything — purely a read-only diagnostic.
pub async fn probe_windows_idle_with(clock: &impl WindowsClock) -> ProbeResult {
    let first = match clock.read() {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult::fail("windows-idle", format!("failed to read idle clock: {e}"));
        }
    };

    tokio::time::sleep(SAMPLE_INTERVAL).await;

    let second = match clock.read() {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult::fail("windows-idle", format!("failed to read idle clock: {e}"));
        }
    };

    if first == second {
        ProbeResult::fail(
            "windows-idle",
            format!(
                "idle clock returned two identical consecutive raw values: {first}, {second} \
                 — expected the value to advance across a {SAMPLE_INTERVAL:?} interval; the \
                 clock may be frozen or unavailable"
            ),
        )
    } else {
        ProbeResult::pass(
            "windows-idle",
            format!("idle clock advanced across the sample interval: {first}, {second}"),
        )
    }
}

// ── Real backend (Windows only) ──────────────────────────────────────────

#[cfg(target_os = "windows")]
mod real {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    use super::WindowsClock;

    /// Real `GetLastInputInfo`-backed clock. See the module docs' "FFI
    /// duplication note" for why this is a separate declaration from
    /// `dormantd::windows_idle`'s own copy.
    pub struct RealWindowsClock;

    impl WindowsClock for RealWindowsClock {
        fn read(&self) -> Result<u32, String> {
            let mut info = LASTINPUTINFO {
                cbSize: u32::try_from(std::mem::size_of::<LASTINPUTINFO>()).unwrap_or(0),
                dwTime: 0,
            };
            // Safety: `info` is a valid, correctly-sized `LASTINPUTINFO` with
            // `cbSize` set as the API requires; `GetLastInputInfo` only writes
            // into it and returns a `BOOL`.
            let ok = unsafe { GetLastInputInfo(&raw mut info) };
            if ok == 0 {
                return Err("GetLastInputInfo returned failure".to_string());
            }
            // Safety: `GetTickCount` takes no arguments and has no failure mode.
            let now = unsafe { GetTickCount() };
            // `wrapping_sub` is mandatory: `GetTickCount` is a 32-bit counter
            // that wraps every ~49.7 days (see `dormantd::windows_idle`'s
            // tested `idle_millis_from_ticks`).
            Ok(now.wrapping_sub(info.dwTime))
        }
    }
}

#[cfg(target_os = "windows")]
pub use real::RealWindowsClock;

/// Probe the real Windows idle clock. Only available on Windows.
#[cfg(target_os = "windows")]
pub async fn probe_windows_idle() -> ProbeResult {
    probe_windows_idle_with(&RealWindowsClock).await
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;
    use std::sync::Mutex;

    /// Scripted clock: returns values from a fixed sequence, FIFO, repeating
    /// the last value once exhausted.
    struct ScriptedWindowsClock {
        values: Vec<u32>,
        idx: Mutex<usize>,
    }

    impl ScriptedWindowsClock {
        fn new(values: Vec<u32>) -> Self {
            Self {
                values,
                idx: Mutex::new(0),
            }
        }
    }

    impl WindowsClock for ScriptedWindowsClock {
        fn read(&self) -> Result<u32, String> {
            let mut idx = self.idx.lock().unwrap();
            let v = self.values[(*idx).min(self.values.len() - 1)];
            *idx += 1;
            Ok(v)
        }
    }

    struct FailingWindowsClock;
    impl WindowsClock for FailingWindowsClock {
        fn read(&self) -> Result<u32, String> {
            Err("simulated read failure".to_string())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn windows_idle_probe_surfaces_consecutive_raw_values() {
        let clock = ScriptedWindowsClock::new(vec![12_250, 12_250]);
        let result = probe_windows_idle_with(&clock).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(
            result.detail.contains("12250, 12250"),
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
    async fn windows_idle_probe_passes_when_clock_advances() {
        let clock = ScriptedWindowsClock::new(vec![1_000, 1_500]);
        let result = probe_windows_idle_with(&clock).await;
        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(result.detail.contains("1000, 1500"), "{}", result.detail);
    }

    #[tokio::test(start_paused = true)]
    async fn windows_idle_probe_fails_on_read_error() {
        let clock = FailingWindowsClock;
        let result = probe_windows_idle_with(&clock).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("simulated read failure"));
    }
}
