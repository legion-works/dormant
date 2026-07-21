//! macOS display-sleep doctor probe — read-only.
//!
//! Reports IOPM/`CoreGraphics` API availability and each *online* display's
//! current asleep/awake state via the SAME readback
//! `dormant_displays::macos_power::RealDisplaySleepTransport::online_sleep_states`
//! that `MacosDisplaySleepController::wake` polls during a real wake — reused
//! here in read-only isolation. This probe NEVER calls `sleep_all` (would
//! blank the display) or `declare_user_activity` (would grab a wake
//! assertion); see Task 11's plan invariant: "NEVER re-runs owned display
//! blank/wake." The probe's own seam trait ([`MacosPowerProbeOps`], below)
//! only ever exposes read methods — those two mutating calls are not even
//! reachable through it, by construction.
//!
//! ## Headless / no window server
//!
//! On a host with no active console session (SSH-only login, screen sharing
//! to the login window, a macOS CI runner with no logged-in GUI user),
//! `CGSessionCopyCurrentDictionary()` returns `NULL` — Apple's documented
//! technique for detecting "no window server session is attached". This
//! probe checks that FIRST, before attempting any per-display
//! `CoreGraphics` call, and reports `Skip` rather than treating the
//! resulting unusable readback as a failure.

// The platform-neutral logic below (`MacosPowerProbeOps`,
// `probe_macos_display_sleep_with`) is only ever reached in production from
// the `#[cfg(target_os = "macos")]`-gated `probe_macos_display_sleep` at
// the bottom of this file — on a non-macOS, non-test build it is genuinely
// unreachable. Mirrors the identical situation (and identical fix) in
// `dormantd::macos_idle`'s own `macos_run`.
#![cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]

use dormant_displays::macos_display_sleep::CGDirectDisplayID;

use crate::types::ProbeResult;

/// Injectable seam over the platform read this probe needs. Deliberately
/// narrower than `dormant_displays::macos_display_sleep::DisplaySleepTransport`
/// (which also exposes `sleep_all`/`declare_user_activity`, the mutating
/// blank/wake calls this read-only probe must never reach) — the real
/// backend below reuses that transport internally, but this probe's own
/// call surface makes it structurally impossible to blank or wake anything.
pub trait MacosPowerProbeOps: Send + Sync {
    /// Whether a window-server session is active (see module docs).
    fn window_server_active(&self) -> bool;

    /// Read the sleep state of every *online* display: `(id, asleep)`
    /// pairs. Only ever called when [`Self::window_server_active`] is
    /// `true`.
    fn per_display_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, String>;
}

/// Probe macOS display-sleep state via `ops`. Read-only.
pub fn probe_macos_display_sleep_with(ops: &impl MacosPowerProbeOps) -> ProbeResult {
    if !ops.window_server_active() {
        return ProbeResult::skip(
            "macos-display-sleep",
            "no window server session is active (headless / SSH-only login) — per-display \
             sleep state is unavailable without one",
        );
    }

    match ops.per_display_states() {
        Err(e) => ProbeResult::fail(
            "macos-display-sleep",
            format!("failed to read per-display sleep state: {e}"),
        ),
        Ok(states) if states.is_empty() => ProbeResult::skip(
            "macos-display-sleep",
            "window server session active but no online displays detected",
        ),
        Ok(states) => {
            let detail = states
                .iter()
                .map(|(id, asleep)| {
                    format!("display {id}: {}", if *asleep { "asleep" } else { "awake" })
                })
                .collect::<Vec<_>>()
                .join(", ");
            ProbeResult::pass("macos-display-sleep", detail)
        }
    }
}

// ── Real backend (macOS only) ────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod real {
    use super::{CGDirectDisplayID, MacosPowerProbeOps};
    use dormant_displays::macos_display_sleep::DisplaySleepTransport;
    use dormant_displays::macos_power::RealDisplaySleepTransport;

    #[allow(non_snake_case)]
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        /// Returns a +1-retained `CFDictionaryRef` describing the current
        /// console session, or `NULL` when no window server session is
        /// attached — Apple's documented technique for detecting a
        /// headless / SSH-only login. Declared as an opaque pointer here
        /// (never dereferenced): this probe only ever checks null-ness and
        /// releases it, it never reads the dictionary's contents.
        fn CGSessionCopyCurrentDictionary() -> *const std::ffi::c_void;
    }

    /// Real backend: checks for an active window-server session, then
    /// delegates the per-display readback to
    /// `dormant_displays::macos_power::RealDisplaySleepTransport` — the
    /// exact same `IOKit`/`CoreGraphics` call `MacosDisplaySleepController::wake`
    /// polls, called here in isolation (no assertion declared, no `pmset
    /// displaysleepnow` spawned).
    pub struct RealMacosPowerProbeOps;

    impl MacosPowerProbeOps for RealMacosPowerProbeOps {
        fn window_server_active(&self) -> bool {
            // Safety: no arguments; the C API documents NULL as "no
            // session" and a valid, +1-retained CFDictionaryRef otherwise.
            let session = unsafe { CGSessionCopyCurrentDictionary() };
            if session.is_null() {
                return false;
            }
            // Safety: `session` was just checked non-null and is a
            // +1-retained Core Foundation object per the "Copy" naming
            // rule — release it exactly once, having never dereferenced it.
            unsafe { core_foundation_sys::base::CFRelease(session.cast()) };
            true
        }

        fn per_display_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, String> {
            RealDisplaySleepTransport
                .online_sleep_states()
                .map_err(|e| e.error)
        }
    }
}

#[cfg(target_os = "macos")]
pub use real::RealMacosPowerProbeOps;

/// Probe the real macOS display-sleep API. Only available on macOS.
#[cfg(target_os = "macos")]
pub async fn probe_macos_display_sleep() -> ProbeResult {
    probe_macos_display_sleep_with(&RealMacosPowerProbeOps)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;
    use async_trait::async_trait;
    use dormant_core::types::CmdFailure;
    use dormant_displays::macos_display_sleep::{AssertionGuard, DisplaySleepTransport};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Test double serving two roles at once:
    /// 1. [`MacosPowerProbeOps`] — this probe's own read seam.
    /// 2. `dormant_displays::macos_display_sleep::DisplaySleepTransport` —
    ///    the FULL transport trait, with call counters on
    ///    `sleep_all`/`declare_user_activity`, so tests can PROVE this
    ///    probe never reaches the blank/wake surface even though nothing in
    ///    `MacosPowerProbeOps`'s own shape exposes those two methods.
    ///    Belt-and-braces: the probe trait's shape already makes those
    ///    calls impossible to reach from `probe_macos_display_sleep_with`;
    ///    this fake proves it empirically too, for anyone who later widens
    ///    the probe trait without noticing the invariant.
    struct FakeMacosPower {
        window_server_active: bool,
        states: Mutex<Result<Vec<(CGDirectDisplayID, bool)>, String>>,
        sleep_all_calls: AtomicU32,
        declare_calls: AtomicU32,
    }

    impl FakeMacosPower {
        fn awake(states: Vec<(CGDirectDisplayID, bool)>) -> Self {
            Self {
                window_server_active: true,
                states: Mutex::new(Ok(states)),
                sleep_all_calls: AtomicU32::new(0),
                declare_calls: AtomicU32::new(0),
            }
        }

        fn no_window_server() -> Self {
            Self {
                window_server_active: false,
                states: Mutex::new(Ok(vec![])),
                sleep_all_calls: AtomicU32::new(0),
                declare_calls: AtomicU32::new(0),
            }
        }

        fn failing(err: &str) -> Self {
            Self {
                window_server_active: true,
                states: Mutex::new(Err(err.to_string())),
                sleep_all_calls: AtomicU32::new(0),
                declare_calls: AtomicU32::new(0),
            }
        }

        fn sleep_all_calls(&self) -> u32 {
            self.sleep_all_calls.load(Ordering::SeqCst)
        }

        fn declare_calls(&self) -> u32 {
            self.declare_calls.load(Ordering::SeqCst)
        }
    }

    impl MacosPowerProbeOps for FakeMacosPower {
        fn window_server_active(&self) -> bool {
            self.window_server_active
        }

        fn per_display_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, String> {
            self.states.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DisplaySleepTransport for FakeMacosPower {
        async fn sleep_all(&self) -> Result<(), CmdFailure> {
            self.sleep_all_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn declare_user_activity(&self) -> Result<AssertionGuard, CmdFailure> {
            self.declare_calls.fetch_add(1, Ordering::SeqCst);
            Ok(AssertionGuard::new(|| {}))
        }

        fn online_sleep_states(&self) -> Result<Vec<(CGDirectDisplayID, bool)>, CmdFailure> {
            self.states.lock().unwrap().clone().map_err(|e| CmdFailure {
                controller: "fake-macos-power".to_string(),
                error: e,
            })
        }
    }

    // ── headless_display_probe_is_skip_not_pass ────────────────────────────

    #[test]
    fn headless_display_probe_is_skip_not_pass() {
        let fake = FakeMacosPower::no_window_server();
        let result = probe_macos_display_sleep_with(&fake);
        assert_eq!(result.status, ProbeStatus::Skip, "{result:?}");
        assert!(result.detail.contains("window server"));
        assert_eq!(
            fake.sleep_all_calls(),
            0,
            "read-only probe must never call sleep_all"
        );
        assert_eq!(
            fake.declare_calls(),
            0,
            "read-only probe must never call declare_user_activity"
        );
    }

    // ── awake / asleep reporting ────────────────────────────────────────────

    #[test]
    fn reports_awake_and_asleep_displays() {
        let fake = FakeMacosPower::awake(vec![(1, false), (2, true)]);
        let result = probe_macos_display_sleep_with(&fake);
        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(
            result.detail.contains("display 1: awake"),
            "{}",
            result.detail
        );
        assert!(
            result.detail.contains("display 2: asleep"),
            "{}",
            result.detail
        );
        assert_eq!(fake.sleep_all_calls(), 0);
        assert_eq!(fake.declare_calls(), 0);
    }

    #[test]
    fn no_online_displays_is_skip() {
        let fake = FakeMacosPower::awake(vec![]);
        let result = probe_macos_display_sleep_with(&fake);
        assert_eq!(result.status, ProbeStatus::Skip, "{result:?}");
        assert!(result.detail.contains("no online displays"));
    }

    #[test]
    fn readback_failure_is_fail() {
        let fake = FakeMacosPower::failing("simulated CoreGraphics failure");
        let result = probe_macos_display_sleep_with(&fake);
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("simulated CoreGraphics failure"));
        assert_eq!(fake.sleep_all_calls(), 0);
        assert_eq!(fake.declare_calls(), 0);
    }
}
