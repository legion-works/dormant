//! macOS `CoreGraphics` idle-time source and frozen-source defense.
//!
//! Two layers, deliberately separated so the defense logic is testable on
//! any platform (RED-first tests below run on Linux CI):
//!
//! * [`MacosIdleGuard`] — pure state machine. Feeds raw
//!   "seconds since last input" samples through malformed/sanity-cap/frozen
//!   checks and returns a [`GuardOutcome`]. No I/O, no `cfg`.
//! * `macos_run` (crate-private) — generic over [`MacosIdleClock`], drives
//!   the guard on the poll cadence and publishes per-rule inhibition via
//!   the same `publish` / `set_all_inactive` / `sleep_or_cancel` helpers
//!   [`crate::idle_source`]'s `DBus` source uses. Also platform-neutral;
//!   takes any `MacosIdleClock` impl, so tests drive it with a scripted
//!   fake clock instead of the real `CoreGraphics` call.
//!
//! The only `#[cfg(target_os = "macos")]`-gated code is the thin FFI layer
//! at the bottom: the `CGEventSourceSecondsSinceLastEventType` extern
//! declaration, the [`MacosIdleClock`] impl backed by it, and the
//! [`crate::idle_source::IdleSource`] impl that wires the two together for
//! production use.
//!
//! DEFERRED: PR CI for the `#[cfg(target_os = "macos")]` section below —
//! it cannot compile or run in the Linux sandbox this task was implemented
//! in, and must be exercised for the first time on the macOS CI lane or
//! real hardware before being trusted. The guard and `macos_run` above it
//! are fully exercised here on Linux.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use dormant_core::rules::ControlMsg;
use dormant_core::types::RuleId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::idle_source::{ActivityRule, publish, set_all_inactive, sleep_or_cancel};

// ── IdleReadError ───────────────────────────────────────────────────────────────

/// Error reading the idle clock. The real `CoreGraphics` call has no error
/// path of its own (`CGEventSourceSecondsSinceLastEventType` always returns
/// a `double`), but the trait keeps a fallible signature so fakes/tests can
/// exercise the fail-toward-inactive path without having to fabricate a
/// "malformed" `f64` for it.
#[derive(Debug, Clone)]
pub struct IdleReadError(pub String);

impl fmt::Display for IdleReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "macos idle clock read failed: {}", self.0)
    }
}

impl std::error::Error for IdleReadError {}

// ── MacosIdleClock ─────────────────────────────────────────────────────────────

/// Abstraction over "seconds since any input event" so the source's poll
/// loop is testable without the real `CoreGraphics` call.
pub trait MacosIdleClock: Send + Sync {
    /// Read the current idle duration, in seconds.
    ///
    /// # Errors
    ///
    /// Returns [`IdleReadError`] when the underlying read fails or is
    /// otherwise unavailable. The real `CoreGraphics`-backed implementation
    /// never fails (the C API has no error path of its own); this exists so
    /// fakes/tests can exercise the fail-toward-inactive path.
    fn seconds_since_any_input(&self) -> Result<f64, IdleReadError>;
}

// ── Guard ───────────────────────────────────────────────────────────────────────

/// Why [`MacosIdleGuard::observe`] rejected a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokenReason {
    /// The sample itself is unusable: negative, NaN, or infinite.
    Malformed,
    /// The sample exceeds [`MacosIdleGuard`]'s configured sanity cap.
    SanityCap,
    /// The clock returned the same bit-identical value for
    /// `frozen_polls` consecutive polls.
    Frozen,
}

/// The guard's verdict for one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GuardOutcome {
    /// The sample is trustworthy. `recovered` is `true` exactly once, on
    /// the first healthy sample after a `Broken` episode (i.e. the warn
    /// latch just reset).
    Healthy {
        /// The idle duration, in seconds, as reported by the clock.
        idle: f64,
        /// Whether this call transitioned out of a warned `Broken` episode.
        recovered: bool,
    },
    /// The sample breached the sanity cap, but we're still within
    /// `startup_grace` of the source starting — `CGEventSource` idle
    /// counters can report stale/pre-launch values immediately after
    /// process start, so this is not (yet) treated as `Broken`.
    StartupGrace,
    /// The source is unhealthy; callers must publish `inhibited = false`
    /// for every rule. `warn` is `true` only on the call that transitions
    /// into this state (the start of a "broken episode") — callers should
    /// log a `idle_source_frozen` warning exactly then, not on every
    /// subsequent broken poll.
    Broken {
        /// Why this sample was rejected.
        reason: BrokenReason,
        /// Whether the caller should log the `idle_source_frozen` warning
        /// for this call (first broken sample in the episode only).
        warn: bool,
    },
}

/// Pure state machine defending the macOS idle source against a frozen,
/// out-of-range, or malformed `CGEventSourceSecondsSinceLastEventType`
/// reading. See the module docs for the split between this (pure) and the
/// source's poll loop (I/O + publish).
///
/// Frozen detection compares **bit-identical** (`f64::to_bits`) consecutive
/// samples, not rounded/equal-duration comparisons — rounding to seconds
/// would turn real subsecond activity (e.g. `0.087` → `0.094` → `0.121`)
/// into a false freeze.
pub struct MacosIdleGuard {
    frozen_polls: usize,
    sanity_cap: Duration,
    startup_grace: Duration,
    last_bits: Option<u64>,
    repeat_count: usize,
    /// Set on entry to `Broken`, cleared on the next `Healthy` sample —
    /// the "warn once per episode" latch.
    warned: bool,
}

impl MacosIdleGuard {
    /// Create a guard. `frozen_polls` is clamped to at least `1` (a value
    /// of `0` would never leave the "not yet frozen" state, which is not a
    /// meaningful configuration — schema validation floors this at `2`
    /// anyway; the clamp here is just defense in depth).
    #[must_use]
    pub fn new(frozen_polls: usize, sanity_cap: Duration, startup_grace: Duration) -> Self {
        Self {
            frozen_polls: frozen_polls.max(1),
            sanity_cap,
            startup_grace,
            last_bits: None,
            repeat_count: 0,
            warned: false,
        }
    }

    /// Feed one sample. `elapsed_since_start` is the time since this guard
    /// (i.e. the source instance) was created — used for the startup-grace
    /// check. A new source instance must use a fresh guard so it never
    /// inherits cached frozen/warned state from a prior run.
    pub fn observe(&mut self, sample: f64, elapsed_since_start: Duration) -> GuardOutcome {
        if !sample.is_finite() || sample < 0.0 {
            self.reset_freeze_tracking();
            return self.mark_broken(BrokenReason::Malformed);
        }

        if sample > self.sanity_cap.as_secs_f64() {
            if elapsed_since_start < self.startup_grace {
                return GuardOutcome::StartupGrace;
            }
            self.reset_freeze_tracking();
            return self.mark_broken(BrokenReason::SanityCap);
        }

        let bits = sample.to_bits();
        if self.last_bits == Some(bits) {
            self.repeat_count += 1;
        } else {
            self.last_bits = Some(bits);
            self.repeat_count = 1;
        }

        if self.repeat_count >= self.frozen_polls {
            return self.mark_broken(BrokenReason::Frozen);
        }

        self.mark_healthy(sample)
    }

    fn reset_freeze_tracking(&mut self) {
        self.last_bits = None;
        self.repeat_count = 0;
    }

    fn mark_broken(&mut self, reason: BrokenReason) -> GuardOutcome {
        let warn = !self.warned;
        self.warned = true;
        GuardOutcome::Broken { reason, warn }
    }

    fn mark_healthy(&mut self, idle: f64) -> GuardOutcome {
        let recovered = self.warned;
        self.warned = false;
        GuardOutcome::Healthy { idle, recovered }
    }
}

// ── MacosIdleGuardConfig ────────────────────────────────────────────────────────

/// The `daemon.macos_idle_*` knobs, bundled for threading from config through
/// [`crate::idle_source::create_source`] to [`MacosIdleGuard::new`]. Plain
/// data — not `cfg`-gated, so non-macOS builds can still construct and pass
/// one through even though it's only ever consulted on macOS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacosIdleGuardConfig {
    /// See [`dormant_core::config::defaults::MACOS_IDLE_FROZEN_POLLS`].
    pub frozen_polls: u32,
    /// See [`dormant_core::config::defaults::MACOS_IDLE_SANITY_CAP`].
    pub sanity_cap: Duration,
    /// See [`dormant_core::config::defaults::MACOS_IDLE_STARTUP_GRACE`].
    pub startup_grace: Duration,
}

impl Default for MacosIdleGuardConfig {
    fn default() -> Self {
        Self {
            frozen_polls: dormant_core::config::defaults::MACOS_IDLE_FROZEN_POLLS,
            sanity_cap: dormant_core::config::defaults::MACOS_IDLE_SANITY_CAP,
            startup_grace: dormant_core::config::defaults::MACOS_IDLE_STARTUP_GRACE,
        }
    }
}

// ── macos_run ───────────────────────────────────────────────────────────────────

/// Poll `clock` on `poll_interval`, drive it through `guard`, and publish
/// per-rule inhibition state via `ctl`.
///
/// Fail-toward-inactive throughout: clock error, malformed sample,
/// sanity-cap breach, startup grace, or frozen source all publish
/// `inhibited = false` for every rule. Only a `Healthy` sample publishes
/// per-rule based on the configured idle threshold.
///
/// Cancellation interrupts the poll sleep (via `sleep_or_cancel`, shared
/// with the `DBus` source).
///
/// Only `MacosIdleSource::run` calls this outside of tests, and that impl
/// is `#[cfg(target_os = "macos")]` — so on a non-macOS, non-test build
/// (e.g. this crate's Linux `cargo check`/`clippy`) this function has no
/// caller at all. That's expected, not dead code to trim: it's kept
/// unconditionally compiled specifically so the RED-first tests above can
/// drive it on Linux CI.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
pub(crate) async fn macos_run<C: MacosIdleClock>(
    clock: C,
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    mut guard: MacosIdleGuard,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    let start = Instant::now();
    let mut last_sent: HashMap<RuleId, bool> = HashMap::new();

    loop {
        match clock.seconds_since_any_input() {
            Ok(sample) => match guard.observe(sample, start.elapsed()) {
                GuardOutcome::Healthy { idle, recovered } => {
                    if recovered {
                        tracing::info!(event = "idle_source_recovered");
                    }
                    let idle_dur = Duration::from_secs_f64(idle);
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
                            "macos idle source unhealthy; treating user as inactive",
                        );
                    }
                    set_all_inactive(&ctl, &mut last_sent, &rules);
                }
            },
            Err(e) => {
                tracing::warn!(
                    event = "activity_inhibitor_probe_failed",
                    error = %e,
                    "macos idle probe failed; treating user as inactive",
                );
                set_all_inactive(&ctl, &mut last_sent, &rules);
            }
        }

        if sleep_or_cancel(poll_interval, &cancel).await {
            return;
        }
    }
}

// ── Production CoreGraphics-backed source ──────────────────────────────────────

/// Thin `extern "C"` surface — the minimum needed to implement
/// [`MacosIdleClock`] over `CGEventSourceSecondsSinceLastEventType`. Kept
/// separate from the platform-neutral logic above per the same "keep it
/// thin" line drawn in `dormant-displays`' `macos_display_catalog.rs`.
#[cfg(target_os = "macos")]
mod ffi {
    /// `CGEventSourceStateID` — `kCGEventSourceStateCombinedSessionState`.
    /// Combines HID system state and any current event-tap session, which
    /// is what "seconds since any input" should reflect.
    pub(super) const K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE: i32 = 0;

    /// `kCGAnyInputEventType` — `(CGEventType)~0`, matches any event type.
    pub(super) const K_CG_ANY_INPUT_EVENT_TYPE: u32 = u32::MAX;

    #[allow(non_snake_case)]
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CGEventSourceSecondsSinceLastEventType(stateID: i32, eventType: u32) -> f64;
    }
}

/// [`MacosIdleClock`] backed by the real `CoreGraphics` call.
#[cfg(target_os = "macos")]
struct CgEventSourceClock;

#[cfg(target_os = "macos")]
impl MacosIdleClock for CgEventSourceClock {
    fn seconds_since_any_input(&self) -> Result<f64, IdleReadError> {
        // Safety: `CGEventSourceSecondsSinceLastEventType` takes two plain
        // integer arguments and returns a `double` by value — no pointers,
        // no ownership to manage. Always safe to call.
        let secs = unsafe {
            ffi::CGEventSourceSecondsSinceLastEventType(
                ffi::K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE,
                ffi::K_CG_ANY_INPUT_EVENT_TYPE,
            )
        };
        Ok(secs)
    }
}

/// The macOS `CoreGraphics` idle source: polls
/// `CGEventSourceSecondsSinceLastEventType` on `poll_interval`, running the
/// reading through a fresh [`MacosIdleGuard`] so a new source instance
/// never inherits cached frozen/warned state from a previous run.
#[cfg(target_os = "macos")]
pub struct MacosIdleSource {
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    guard_cfg: MacosIdleGuardConfig,
}

#[cfg(target_os = "macos")]
impl MacosIdleSource {
    /// Create a macOS idle source.
    #[must_use]
    pub fn new(
        rules: Vec<ActivityRule>,
        poll_interval: Duration,
        guard_cfg: MacosIdleGuardConfig,
    ) -> Self {
        Self {
            rules,
            poll_interval,
            guard_cfg,
        }
    }
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl crate::idle_source::IdleSource for MacosIdleSource {
    async fn run(self: Box<Self>, ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
        let guard = MacosIdleGuard::new(
            usize::try_from(self.guard_cfg.frozen_polls).unwrap_or(usize::MAX),
            self.guard_cfg.sanity_cap,
            self.guard_cfg.startup_grace,
        );
        macos_run(
            CgEventSourceClock,
            self.rules,
            self.poll_interval,
            guard,
            ctl,
            cancel,
        )
        .await;
    }
}

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
    use crate::macos_idle::{
        BrokenReason, GuardOutcome, IdleReadError, MacosIdleClock, MacosIdleGuard, macos_run,
    };

    /// Test double for [`MacosIdleClock`] — replays a scripted sequence of
    /// samples in order.
    struct ScriptedClock {
        samples: Mutex<VecDeque<Result<f64, IdleReadError>>>,
    }

    impl ScriptedClock {
        fn new(samples: Vec<Result<f64, IdleReadError>>) -> Self {
            Self {
                samples: Mutex::new(samples.into()),
            }
        }
    }

    impl MacosIdleClock for ScriptedClock {
        fn seconds_since_any_input(&self) -> Result<f64, IdleReadError> {
            self.samples
                .lock()
                .expect("scripted clock mutex poisoned")
                .pop_front()
                .unwrap_or(Ok(0.0))
        }
    }

    const FROZEN_POLLS: usize = 3;
    const SANITY_CAP: Duration = Duration::from_secs(24 * 60 * 60);
    const STARTUP_GRACE: Duration = Duration::from_secs(15);
    /// Elapsed time well past startup grace, for tests that don't care about it.
    const PAST_GRACE: Duration = Duration::from_secs(3600);

    #[test]
    fn third_bit_identical_sample_marks_source_frozen_once() {
        let mut guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);

        assert!(matches!(
            guard.observe(0.0, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));
        assert!(matches!(
            guard.observe(0.0, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));
        assert_eq!(
            guard.observe(0.0, PAST_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: true
            }
        );
        assert_eq!(
            guard.observe(0.0, PAST_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: false
            }
        );
    }

    #[test]
    fn growing_idle_and_varying_subsecond_activity_do_not_freeze() {
        let mut guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);
        for sample in [35.0, 40.0, 45.0, 0.087, 0.094, 0.121] {
            let outcome = guard.observe(sample, PAST_GRACE);
            assert!(
                matches!(outcome, GuardOutcome::Healthy { .. }),
                "sample {sample} should stay healthy, got {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn broken_clock_publishes_inactive_for_every_rule() {
        struct AlwaysErr;
        impl MacosIdleClock for AlwaysErr {
            fn seconds_since_any_input(&self) -> Result<f64, IdleReadError> {
                Err(IdleReadError("boom".into()))
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
            macos_run(
                AlwaysErr,
                rules,
                Duration::from_millis(5),
                guard,
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

    #[test]
    fn active_then_frozen_source_must_publish_inactive_and_recover_warning_latch() {
        // Pure-guard half: verify the warn latch fires exactly once across
        // the freeze episode and resets on recovery. (No repo-wide tracing
        // log-capture harness exists for dormantd today — see report notes
        // — so the guard's own returned `warn`/`recovered` bits are the
        // counter seam this test asserts against.)
        let mut guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);
        let samples = [0.125_f64, 0.0, 0.0, 0.0, 1.0];
        let outcomes: Vec<GuardOutcome> = samples
            .iter()
            .map(|&s| guard.observe(s, PAST_GRACE))
            .collect();

        assert!(matches!(outcomes[0], GuardOutcome::Healthy { .. }));
        assert!(matches!(outcomes[1], GuardOutcome::Healthy { .. }));
        assert!(matches!(outcomes[2], GuardOutcome::Healthy { .. }));
        assert_eq!(
            outcomes[3],
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: true
            }
        );
        assert_eq!(
            outcomes[4],
            GuardOutcome::Healthy {
                idle: 1.0,
                recovered: true
            }
        );
    }

    #[test]
    fn warn_once_latch_resets_after_recovery_across_multiple_freeze_episodes() {
        // Regression for the warn-once latch's *reset*, not just its "once
        // per episode" behavior: a second freeze episode, after a clean
        // recovery, must warn again on its first Broken sample. A guard
        // that forgot to clear `warned` in `mark_healthy` on recovery would
        // still pass the single-episode test above
        // (`active_then_frozen_source_must_publish_inactive_and_recover_warning_latch`)
        // while silently leaking `warned = true` across episodes — this
        // drives the guard through two full freeze episodes with a healthy
        // gap between them and asserts `warn` on each episode's first
        // `Broken` sample.
        let mut guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);

        // Pre-freeze healthy baseline.
        assert!(matches!(
            guard.observe(0.125, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));

        // Episode 1: three bit-identical samples freeze the source; only
        // the first Broken call carries `warn: true`.
        assert!(matches!(
            guard.observe(1.0, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));
        assert!(matches!(
            guard.observe(1.0, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));
        assert_eq!(
            guard.observe(1.0, PAST_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: true
            },
            "episode 1 first Broken sample must warn"
        );
        assert_eq!(
            guard.observe(1.0, PAST_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: false
            },
            "episode 1 subsequent Broken sample must not re-warn"
        );

        // Full recovery: a fresh (non-repeating) sample clears the latch
        // and reports `recovered: true` exactly once.
        assert_eq!(
            guard.observe(2.0, PAST_GRACE),
            GuardOutcome::Healthy {
                idle: 2.0,
                recovered: true
            },
            "first healthy sample after episode 1 must report recovered"
        );
        // A second, unrelated healthy sample: the latch is already reset,
        // so this one must NOT report recovered again.
        assert_eq!(
            guard.observe(3.0, PAST_GRACE),
            GuardOutcome::Healthy {
                idle: 3.0,
                recovered: false
            },
            "steady-state healthy sample must not report recovered"
        );

        // Episode 2: freeze again, on a different repeated value. If
        // `warned` were never cleared on recovery, this call would come
        // back `warn: false` because the guard would think it already
        // warned back in episode 1.
        assert!(matches!(
            guard.observe(9.0, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));
        assert!(matches!(
            guard.observe(9.0, PAST_GRACE),
            GuardOutcome::Healthy { .. }
        ));
        assert_eq!(
            guard.observe(9.0, PAST_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: true
            },
            "episode 2 first Broken sample must ALSO warn"
        );
        assert_eq!(
            guard.observe(9.0, PAST_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::Frozen,
                warn: false
            },
            "episode 2 subsequent Broken sample must not re-warn"
        );
    }

    #[tokio::test]
    async fn active_then_frozen_source_publishes_inactive_then_recovers() {
        // Source-level half of the same scenario: verify the publish
        // sequence over the control channel.
        let rules = vec![ActivityRule {
            rule: RuleId("r".into()),
            idle_threshold: Duration::from_secs(2),
        }];
        let clock = ScriptedClock::new(vec![Ok(0.125), Ok(0.0), Ok(0.0), Ok(0.0), Ok(1.0)]);
        let (ctl, mut ctl_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            macos_run(
                clock,
                rules,
                Duration::from_millis(5),
                guard,
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
    async fn malformed_samples_force_inactive_after_active_state() {
        for bad in [-1.0_f64, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let rules = vec![ActivityRule {
                rule: RuleId("r".into()),
                idle_threshold: Duration::from_secs(2),
            }];
            let clock = ScriptedClock::new(vec![Ok(0.125), Ok(bad)]);
            let (ctl, mut ctl_rx) = mpsc::channel(16);
            let cancel = CancellationToken::new();
            let guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);

            let cancel_clone = cancel.clone();
            let handle = tokio::spawn(async move {
                macos_run(
                    clock,
                    rules,
                    Duration::from_millis(5),
                    guard,
                    ctl,
                    cancel_clone,
                )
                .await;
            });

            let mut msgs = Vec::new();
            for _ in 0..2 {
                if let Some(ControlMsg::SetInhibited { inhibited, .. }) = ctl_rx.recv().await {
                    msgs.push(inhibited);
                }
            }
            cancel.cancel();
            handle.await.ok();

            assert_eq!(
                msgs,
                vec![true, false],
                "bad sample {bad:?} should force inactive after active"
            );
        }
    }

    #[test]
    fn above_sanity_cap_is_broken_after_startup_grace() {
        let mut guard = MacosIdleGuard::new(FROZEN_POLLS, SANITY_CAP, STARTUP_GRACE);
        assert_eq!(
            guard.observe(90_000.0, Duration::ZERO),
            GuardOutcome::StartupGrace
        );
        assert_eq!(
            guard.observe(90_000.0, STARTUP_GRACE),
            GuardOutcome::Broken {
                reason: BrokenReason::SanityCap,
                warn: true
            }
        );
    }
}
