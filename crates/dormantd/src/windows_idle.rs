//! Windows `GetLastInputInfo` idle-time source and frozen-source defense.
//!
//! Deliberately symmetric with [`crate::macos_idle`] — same two-layer split,
//! same guard, same poll loop shape — because both are polled OS idle-time
//! APIs with the same failure modes (a frozen counter, an insane reading, a
//! stale value right after process start):
//!
//! * [`WindowsIdleClock`] — the injectable seam over "milliseconds since any
//!   input event", so the poll loop is testable on ANY host without the
//!   Windows API. The real `GetLastInputInfo`-backed impl is `cfg`-gated at
//!   the bottom.
//! * `windows_run` (crate-private) — generic over [`WindowsIdleClock`], drives
//!   the shared [`crate::macos_idle::MacosIdleGuard`] on the poll cadence and
//!   publishes per-rule inhibition via the same `publish` / `set_all_inactive`
//!   / `sleep_or_cancel` helpers the `DBus` and macOS sources use. Also
//!   platform-neutral; tests drive it with a scripted fake clock.
//!
//! The guard is reused verbatim from `macos_idle` rather than duplicated: it
//! is a pure state machine over "seconds since last input" samples and has no
//! macOS-specific behavior. Its `daemon.macos_idle_*` config knobs
//! ([`crate::macos_idle::MacosIdleGuardConfig`]) therefore also govern the
//! Windows source — the knobs are generic idle-guard tunables, only the key
//! prefix is historical.
//!
//! ## The 32-bit tick wrap
//!
//! `GetLastInputInfo` fills `dwTime` from `GetTickCount`, a **32-bit** tick
//! count that wraps every ~49.7 days. The idle duration is
//! `GetTickCount().wrapping_sub(dwTime)` — a plain subtraction underflows
//! across a wrap (panic in debug, a garbage multi-week idle in release, which
//! would blank a display mid-use). [`idle_millis_from_ticks`] is the single
//! place that arithmetic lives, and it is tested on Linux. `GetTickCount64`
//! is deliberately NOT used: mixing a 64-bit `now` with the 32-bit `dwTime`
//! reintroduces exactly the wrap bug.
//!
//! ## Session / elevation limitation
//!
//! `GetLastInputInfo` reports input for the **calling session only**, and a
//! non-elevated process does not see input delivered to elevated windows
//! (UIPI). An operator running mixed-elevation apps may therefore observe the
//! idle time advance while they are actively typing into an elevated window.
//! This is a documented limitation of the API, not a bug this source can
//! paper over.
//!
//! DEFERRED: the `#[cfg(target_os = "windows")]` FFI section below cannot
//! compile or run in the Linux sandbox this was implemented in; it is
//! exercised for the first time on the Windows CI lane or real hardware. The
//! clock seam, the wrap arithmetic, and `windows_run` above it are fully
//! exercised here on Linux.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use dormant_core::rules::ControlMsg;
use dormant_core::types::RuleId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::idle_source::{ActivityRule, publish, set_all_inactive, sleep_or_cancel};
#[cfg(target_os = "windows")]
use crate::macos_idle::MacosIdleGuardConfig;
use crate::macos_idle::{GuardOutcome, MacosIdleGuard};

// ── WindowsIdleReadError ────────────────────────────────────────────────────────

/// Error reading the Windows idle clock. `GetLastInputInfo` can fail (it
/// returns `FALSE`), so unlike the macOS `CoreGraphics` call this path is
/// reachable in production; the trait keeps a fallible signature so fakes can
/// exercise the fail-toward-inactive path too.
#[derive(Debug, Clone)]
pub struct WindowsIdleReadError(pub String);

impl fmt::Display for WindowsIdleReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "windows idle clock read failed: {}", self.0)
    }
}

impl std::error::Error for WindowsIdleReadError {}

// ── WindowsIdleClock ────────────────────────────────────────────────────────────

/// Abstraction over "milliseconds since any input event" so the source's poll
/// loop is testable without the real `GetLastInputInfo` call.
pub trait WindowsIdleClock: Send + Sync {
    /// Read the current idle duration, in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsIdleReadError`] when the underlying read fails. The
    /// real `GetLastInputInfo`-backed implementation returns this when the
    /// API reports failure.
    fn idle_millis(&self) -> Result<u32, WindowsIdleReadError>;
}

// ── Wrap-safe tick subtraction ──────────────────────────────────────────────────

/// Idle milliseconds from two `GetTickCount`-domain tick values.
///
/// `wrapping_sub` is mandatory: `GetTickCount` is a 32-bit counter that wraps
/// every ~49.7 days, so `now` can be numerically smaller than `last_input`
/// across a wrap. A plain subtraction would underflow — panic in debug, a
/// garbage multi-week idle in release. Kept as a free function so the wrap
/// case is testable on every platform.
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
fn idle_millis_from_ticks(now: u32, last_input: u32) -> u32 {
    now.wrapping_sub(last_input)
}

// ── windows_run ─────────────────────────────────────────────────────────────────

/// Poll `clock` on `poll_interval`, drive it through `guard`, and publish
/// per-rule inhibition state via `ctl`.
///
/// Fail-toward-inactive throughout: clock error, malformed sample,
/// sanity-cap breach, startup grace, or frozen source all publish
/// `inhibited = false` for every rule. Only a `Healthy` sample publishes
/// per-rule based on the configured idle threshold.
///
/// Cancellation interrupts the poll sleep (via `sleep_or_cancel`, shared with
/// the `DBus` and macOS sources).
///
/// Only `WindowsIdleSource::run` calls this outside of tests, and that impl is
/// `#[cfg(target_os = "windows")]` — so on a non-Windows, non-test build this
/// function has no caller at all. That's expected, not dead code to trim: it
/// is kept unconditionally compiled specifically so the tests below can drive
/// it on Linux CI.
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
pub(crate) async fn windows_run<C: WindowsIdleClock>(
    clock: C,
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    mut guard: MacosIdleGuard,
    idle_tx: Option<crate::idle_observation::IdleObservationTx>,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    let start = Instant::now();
    let mut last_sent: HashMap<RuleId, bool> = HashMap::new();

    loop {
        match clock.idle_millis() {
            Ok(ms) => {
                // The guard works in seconds; the Windows clock reports
                // milliseconds. Convert at the boundary so the shared guard's
                // sanity cap (a `Duration`) compares in the unit it expects.
                let sample = f64::from(ms) / 1000.0;
                match guard.observe(sample, start.elapsed()) {
                    GuardOutcome::Healthy { idle, recovered } => {
                        if recovered {
                            tracing::info!(event = "idle_source_recovered");
                        }
                        let idle_dur = Duration::from_secs_f64(idle);
                        if let Some(ref tx) = idle_tx {
                            let now = Instant::now();
                            let last_activity = now.checked_sub(idle_dur);
                            let _ = tx.send(crate::idle_observation::IdleObservation {
                                last_activity,
                                observed_at: now,
                                available: true,
                            });
                        }
                        for r in &rules {
                            let inhibited = idle_dur < r.idle_threshold;
                            publish(&ctl, &mut last_sent, &r.rule, inhibited);
                        }
                    }
                    GuardOutcome::StartupGrace => {
                        set_all_inactive(&ctl, &mut last_sent, &rules);
                    }
                    GuardOutcome::Broken { reason, warn } => {
                        if warn {
                            tracing::warn!(
                                event = "idle_source_frozen",
                                reason = ?reason,
                                "windows idle source unhealthy; treating user as inactive",
                            );
                        }
                        set_all_inactive(&ctl, &mut last_sent, &rules);
                        if let Some(ref tx) = idle_tx {
                            let _ = tx.send(crate::idle_observation::IdleObservation {
                                last_activity: None,
                                observed_at: Instant::now(),
                                available: false,
                            });
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "activity_inhibitor_probe_failed",
                    error = %e,
                    "windows idle probe failed; treating user as inactive",
                );
                set_all_inactive(&ctl, &mut last_sent, &rules);
                if let Some(ref tx) = idle_tx {
                    let _ = tx.send(crate::idle_observation::IdleObservation {
                        last_activity: None,
                        observed_at: Instant::now(),
                        available: false,
                    });
                }
            }
        }

        if sleep_or_cancel(poll_interval, &cancel).await {
            return;
        }
    }
}

// ── Production GetLastInputInfo-backed source ───────────────────────────────────

/// Thin `windows-sys` surface — the minimum needed to implement
/// [`WindowsIdleClock`] over `GetLastInputInfo` + `GetTickCount`. Kept
/// separate from the platform-neutral logic above per the same "keep it thin"
/// line drawn in `macos_idle`'s `ffi` module.
#[cfg(target_os = "windows")]
mod ffi {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    use super::{WindowsIdleReadError, idle_millis_from_ticks};

    /// Read the current idle duration, in milliseconds, from the OS.
    pub(super) fn read_idle_millis() -> Result<u32, WindowsIdleReadError> {
        let mut info = LASTINPUTINFO {
            cbSize: u32::try_from(std::mem::size_of::<LASTINPUTINFO>()).unwrap_or(0),
            dwTime: 0,
        };
        // Safety: `info` is a valid, correctly-sized `LASTINPUTINFO` with
        // `cbSize` set as the API requires; `GetLastInputInfo` only writes
        // into it and returns a `BOOL`.
        let ok = unsafe { GetLastInputInfo(&raw mut info) };
        if ok == 0 {
            return Err(WindowsIdleReadError(
                "GetLastInputInfo returned failure".into(),
            ));
        }
        // Safety: `GetTickCount` takes no arguments and has no failure mode.
        let now = unsafe { GetTickCount() };
        Ok(idle_millis_from_ticks(now, info.dwTime))
    }
}

/// [`WindowsIdleClock`] backed by the real `GetLastInputInfo` call.
#[cfg(target_os = "windows")]
struct GetLastInputInfoClock;

#[cfg(target_os = "windows")]
impl WindowsIdleClock for GetLastInputInfoClock {
    fn idle_millis(&self) -> Result<u32, WindowsIdleReadError> {
        ffi::read_idle_millis()
    }
}

/// The Windows `GetLastInputInfo` idle source: polls the OS idle counter on
/// `poll_interval`, running each reading through a fresh [`MacosIdleGuard`]
/// so a new source instance never inherits cached frozen/warned state from a
/// previous run.
#[cfg(target_os = "windows")]
pub struct WindowsIdleSource {
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    guard_cfg: MacosIdleGuardConfig,
    /// Daemon-lifetime idle-observation channel for the activity-claim policy.
    idle_tx: Option<crate::idle_observation::IdleObservationTx>,
}

#[cfg(target_os = "windows")]
impl WindowsIdleSource {
    /// Create a Windows idle source.
    #[must_use]
    pub fn new(
        rules: Vec<ActivityRule>,
        poll_interval: Duration,
        guard_cfg: MacosIdleGuardConfig,
        idle_tx: Option<crate::idle_observation::IdleObservationTx>,
    ) -> Self {
        Self {
            rules,
            poll_interval,
            guard_cfg,
            idle_tx,
        }
    }
}

#[cfg(target_os = "windows")]
#[async_trait::async_trait]
impl crate::idle_source::IdleSource for WindowsIdleSource {
    async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
        let guard = MacosIdleGuard::new(
            usize::try_from(self.guard_cfg.frozen_polls).unwrap_or(usize::MAX),
            self.guard_cfg.sanity_cap,
            self.guard_cfg.startup_grace,
        );
        windows_run(
            GetLastInputInfoClock,
            self.rules,
            self.poll_interval,
            guard,
            self.idle_tx,
            ctl,
            cancel,
        )
        .await;
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use dormant_core::rules::ControlMsg;
    use dormant_core::types::RuleId;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::idle_source::ActivityRule;
    use crate::macos_idle::MacosIdleGuard;
    use crate::windows_idle::{
        WindowsIdleClock, WindowsIdleReadError, idle_millis_from_ticks, windows_run,
    };

    /// Test double for [`WindowsIdleClock`] — replays a scripted sequence of
    /// millisecond samples in order.
    struct ScriptedClock {
        samples: Mutex<VecDeque<Result<u32, WindowsIdleReadError>>>,
    }

    impl ScriptedClock {
        fn new(samples: Vec<Result<u32, WindowsIdleReadError>>) -> Self {
            Self {
                samples: Mutex::new(samples.into()),
            }
        }
    }

    impl WindowsIdleClock for ScriptedClock {
        fn idle_millis(&self) -> Result<u32, WindowsIdleReadError> {
            self.samples
                .lock()
                .expect("scripted clock mutex poisoned")
                .pop_front()
                .unwrap_or(Ok(0))
        }
    }

    const FROZEN_POLLS: usize = 3;
    const SANITY_CAP: Duration = Duration::from_secs(24 * 60 * 60);
    const STARTUP_GRACE: Duration = Duration::from_secs(15);

    // ── Wrap arithmetic ──────────────────────────────────────────────────────

    /// The non-negotiable one: `GetTickCount` wraps every ~49.7 days. With
    /// `dwTime` just below `u32::MAX` and `now` just after the wrap, the idle
    /// duration must be the small real interval — a plain `now - dwTime`
    /// would underflow (panic in debug / garbage in release).
    #[test]
    fn tick_wrap_yields_small_positive_idle() {
        let last_input = u32::MAX - 5; // 4294967290
        let now = 5; // wrapped past u32::MAX
        assert_eq!(
            idle_millis_from_ticks(now, last_input),
            11,
            "across a tick wrap the idle must be the small real interval, not an underflow"
        );
    }

    #[test]
    fn tick_subtraction_without_wrap_is_plain_difference() {
        assert_eq!(idle_millis_from_ticks(1_000, 400), 600);
        assert_eq!(idle_millis_from_ticks(400, 400), 0);
    }

    // ── Guard behaviour through the poll loop (platform-neutral) ─────────────

    #[tokio::test]
    async fn frozen_clock_publishes_inactive_then_recovers() {
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(2),
        }];
        // 125ms active, then three bit-identical 0ms samples (freeze), then a
        // fresh 1000ms sample (recovery).
        let clock = ScriptedClock::new(vec![Ok(125), Ok(0), Ok(0), Ok(0), Ok(1_000)]);
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            windows_run(
                clock,
                rules,
                Duration::from_millis(5),
                guard,
                None,
                ctl,
                cancel_clone,
            )
            .await;
        });

        let mut msgs = Vec::new();
        for _ in 0..3 {
            if let Some(ControlMsg::SetInhibited { inhibited, .. }) = ctl_rx.recv().await {
                msgs.push(inhibited);
            }
        }
        cancel.cancel();
        handle.await.ok();

        assert_eq!(msgs, vec![true, false, true]);
    }

    #[tokio::test]
    async fn sanity_cap_breach_publishes_inactive() {
        // Zero startup grace so the cap is enforced immediately; a 5s sample
        // against a 1s cap must be rejected as Broken(SanityCap).
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let clock = ScriptedClock::new(vec![Ok(5_000)]);
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let guard = MacosIdleGuard::new(FROZEN_POLLS, Duration::from_secs(1), Duration::ZERO);

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            windows_run(
                clock,
                rules,
                Duration::from_millis(5),
                guard,
                None,
                ctl,
                cancel_clone,
            )
            .await;
        });

        let msg = ctl_rx.recv().await;
        cancel.cancel();
        handle.await.ok();

        assert!(
            matches!(
                msg,
                Some(ControlMsg::SetInhibited {
                    inhibited: false,
                    ..
                })
            ),
            "a sanity-cap breach must publish inactive, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn startup_grace_publishes_inactive() {
        // A huge sample within the startup grace window is not yet Broken,
        // but must still fail toward inactive.
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(120),
        }];
        let clock = ScriptedClock::new(vec![Ok(5_000)]);
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let guard = MacosIdleGuard::new(
            FROZEN_POLLS,
            Duration::from_secs(1),
            Duration::from_secs(3600),
        );

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            windows_run(
                clock,
                rules,
                Duration::from_millis(5),
                guard,
                None,
                ctl,
                cancel_clone,
            )
            .await;
        });

        let msg = ctl_rx.recv().await;
        cancel.cancel();
        handle.await.ok();

        assert!(
            matches!(
                msg,
                Some(ControlMsg::SetInhibited {
                    inhibited: false,
                    ..
                })
            ),
            "startup grace must publish inactive, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn broken_clock_publishes_inactive_for_every_rule() {
        struct AlwaysErr;
        impl WindowsIdleClock for AlwaysErr {
            fn idle_millis(&self) -> Result<u32, WindowsIdleReadError> {
                Err(WindowsIdleReadError("boom".into()))
            }
        }

        let rules = vec![
            ActivityRule {
                rule: RuleId("a".into()),
                idle_threshold: Duration::from_secs(120),
            },
            ActivityRule {
                rule: RuleId("b".into()),
                idle_threshold: Duration::from_secs(60),
            },
        ];
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            windows_run(
                AlwaysErr,
                rules,
                Duration::from_millis(5),
                guard,
                None,
                ctl,
                cancel_clone,
            )
            .await;
        });

        let mut seen: std::collections::HashMap<RuleId, bool> = std::collections::HashMap::new();
        for _ in 0..2 {
            if let Some(ControlMsg::SetInhibited {
                rule: Some(r),
                inhibited,
                ..
            }) = ctl_rx.recv().await
            {
                seen.insert(r, inhibited);
            }
        }
        cancel.cancel();
        handle.await.ok();

        assert_eq!(seen.get(&RuleId("a".into())), Some(&false));
        assert_eq!(seen.get(&RuleId("b".into())), Some(&false));
    }
}
