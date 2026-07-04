//! User-activity inhibitor.
//!
//! Polls `org.freedesktop.ScreenSaver.GetSessionIdleTime` on the session bus
//! and publishes rule-level inhibition into the rules engine via
//! [`ControlMsg::SetInhibited`]. One poller runs at the minimum of the
//! declaring rules' `activity_poll_interval`; each rule is evaluated against
//! its own `activity_idle_threshold`.
//!
//! ## Idle-value units
//!
//! `GetSessionIdleTime` is not portable: the freedesktop `ScreenSaver` XML
//! contract documents **seconds**, but current KDE `kscreenlocker` (backed by
//! `KIdleTime`) returns **milliseconds**. `daemon.idle_time_unit` selects the
//! interpretation (`"ms"` / `"s"`), or `"auto"` (default) detects it at
//! runtime: while a value is undetermined the inhibitor treats the user as
//! **inactive** (fail toward blanking) and, once two consecutive polls show a
//! delta ≈ the poll interval, it locks in the unit. Every poll logs the raw
//! value under `idle_probe_raw` with both interpretations. Live verification
//! belongs to `dormantctl doctor kwin` (Task 18) / Spike B.
//!
//! ## Fail-toward-normal-blanking
//!
//! If the session bus is unreachable, the method is unsupported, or the reply
//! errors, the inhibitor treats the user as **inactive** rather than holding
//! screens on forever — a broken idle probe must never wedge displays awake;
//! the sensor/zone layer still guards actual presence. The connection is
//! retried every 60s and the failure is warned once.

use std::collections::HashMap;
use std::time::Duration;

use dormant_core::config::IdleTimeUnit;
use dormant_core::rules::ControlMsg;
use dormant_core::types::RuleId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Reconnect / retry interval after a session-bus failure.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(60);

/// One rule that declares the `user-activity` inhibitor, with its idle
/// threshold.
#[derive(Debug, Clone)]
pub struct ActivityRule {
    /// The rule this inhibitor gates.
    pub rule: RuleId,
    /// Idle time below which the user is considered active (inhibited).
    pub idle_threshold: Duration,
}

/// The resolved interpretation of a raw idle value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedUnit {
    /// Raw value is milliseconds.
    Ms,
    /// Raw value is seconds.
    Secs,
}

impl ResolvedUnit {
    /// Convert a raw idle reading into milliseconds.
    fn to_ms(self, raw: u64) -> u64 {
        match self {
            Self::Ms => raw,
            Self::Secs => raw.saturating_mul(1000),
        }
    }
}

/// Map the configured unit to a resolved unit, or `None` for `auto`.
fn configured_unit(unit: IdleTimeUnit) -> Option<ResolvedUnit> {
    match unit {
        IdleTimeUnit::Auto => None,
        IdleTimeUnit::Ms => Some(ResolvedUnit::Ms),
        IdleTimeUnit::Secs => Some(ResolvedUnit::Secs),
    }
}

/// Number of corroborating idle-growth samples required before locking the
/// unit under `auto`. Guards against a single active-user jitter delta
/// (e.g. 10→13) that happens to land near the poll interval in one unit.
const CORROBORATION: u32 = 2;

/// Classify a single inter-poll delta: it must fall within ±25% of the poll
/// interval in **exactly one** unit interpretation. Deltas matching both or
/// neither (tiny jitter, huge jumps) are inconclusive.
fn classify_delta(delta: u64, poll: Duration) -> Option<ResolvedUnit> {
    let ms = u64::try_from(poll.as_millis()).unwrap_or(u64::MAX);
    let secs = poll.as_secs();
    let ms_match = within_25(delta, ms);
    let secs_match = secs > 0 && within_25(delta, secs);
    match (ms_match, secs_match) {
        (true, false) => Some(ResolvedUnit::Ms),
        (false, true) => Some(ResolvedUnit::Secs),
        _ => None,
    }
}

/// Whether `a` is within `[0.75·b, 1.25·b]` (i.e. `4a ∈ [3b, 5b]`).
fn within_25(a: u64, b: u64) -> bool {
    if b == 0 {
        return false;
    }
    let four_a = a.saturating_mul(4);
    four_a >= b.saturating_mul(3) && four_a <= b.saturating_mul(5)
}

/// Detects (or is told) the idle-value unit. Under `auto` it requires
/// [`CORROBORATION`] consecutive samples agreeing on the same unit before
/// locking; a conflicting or inconclusive sample resets the streak. Until a
/// unit is locked the caller keeps the inhibitor inactive.
#[derive(Debug)]
struct UnitDetector {
    poll: Duration,
    prev_raw: Option<u64>,
    streak: Option<(ResolvedUnit, u32)>,
    locked: Option<ResolvedUnit>,
}

impl UnitDetector {
    fn new(poll: Duration, forced: Option<ResolvedUnit>) -> Self {
        Self {
            poll,
            prev_raw: None,
            streak: None,
            locked: forced,
        }
    }

    /// Feed a raw sample; returns the locked unit once determined.
    fn observe(&mut self, raw: u64) -> Option<ResolvedUnit> {
        if let Some(u) = self.locked {
            return Some(u);
        }
        let candidate = self
            .prev_raw
            .map(|prev| raw.saturating_sub(prev))
            .and_then(|delta| classify_delta(delta, self.poll));
        self.prev_raw = Some(raw);

        match candidate {
            Some(unit) => {
                let count = match self.streak {
                    Some((u, n)) if u == unit => n + 1,
                    _ => 1,
                };
                self.streak = Some((unit, count));
                if count >= CORROBORATION {
                    self.locked = Some(unit);
                }
            }
            None => self.streak = None,
        }
        self.locked
    }
}

/// Spawn the activity-inhibitor poller.
///
/// Returns `None` (spawning nothing) when no rule declares `user-activity`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn spawn(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    unit: IdleTimeUnit,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if rules.is_empty() {
        return None;
    }
    Some(tokio::spawn(run(rules, poll_interval, unit, ctl, cancel)))
}

/// Non-Linux stub — there is no session-bus screensaver service to poll.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn spawn(
    _rules: Vec<ActivityRule>,
    _poll_interval: Duration,
    _unit: IdleTimeUnit,
    _ctl: mpsc::Sender<ControlMsg>,
    _cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    None
}

#[cfg(target_os = "linux")]
async fn run(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    unit: IdleTimeUnit,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) {
    let mut conn: Option<zbus::Connection> = None;
    let mut last_sent: HashMap<RuleId, bool> = HashMap::new();
    let mut detector = UnitDetector::new(poll_interval, configured_unit(unit));
    let mut warned_offline = false;
    let mut warned_undetermined = false;

    loop {
        // Establish (or re-establish) the session-bus connection.
        if conn.is_none() {
            match zbus::Connection::session().await {
                Ok(c) => {
                    conn = Some(c);
                    warned_offline = false;
                }
                Err(e) => {
                    if !warned_offline {
                        tracing::warn!(
                            event = "activity_inhibitor_unreachable",
                            error = %e,
                            "session bus unreachable; treating user as inactive, retry in 60s",
                        );
                        warned_offline = true;
                    }
                    set_all_inactive(&ctl, &mut last_sent, &rules);
                    if sleep_or_cancel(RECONNECT_INTERVAL, &cancel).await {
                        return;
                    }
                    continue;
                }
            }
        }

        match get_idle_raw(conn.as_ref().expect("connection present")).await {
            Ok(raw) => {
                tracing::info!(
                    event = "idle_probe_raw",
                    raw = raw,
                    interp_ms = raw,
                    interp_s = raw,
                );

                let was_locked = detector.locked.is_some();
                if let Some(unit) = detector.observe(raw) {
                    if !was_locked {
                        tracing::info!(event = "idle_unit_determined", unit = ?unit);
                    }
                    let idle = Duration::from_millis(unit.to_ms(raw));
                    for r in &rules {
                        let inhibited = idle < r.idle_threshold;
                        publish(&ctl, &mut last_sent, &r.rule, inhibited);
                    }
                } else {
                    if !warned_undetermined {
                        tracing::warn!(
                            event = "idle_unit_undetermined",
                            "idle-time unit not yet determined; treating user as inactive",
                        );
                        warned_undetermined = true;
                    }
                    set_all_inactive(&ctl, &mut last_sent, &rules);
                }
            }
            Err(e) => {
                if !warned_offline {
                    tracing::warn!(
                        event = "activity_inhibitor_probe_failed",
                        error = %e,
                        "idle probe failed or unsupported; treating user as inactive, retry in 60s",
                    );
                    warned_offline = true;
                }
                conn = None;
                set_all_inactive(&ctl, &mut last_sent, &rules);
                if sleep_or_cancel(RECONNECT_INTERVAL, &cancel).await {
                    return;
                }
                continue;
            }
        }

        if sleep_or_cancel(poll_interval, &cancel).await {
            return;
        }
    }
}

/// Publish `inhibited = false` for every rule (fail-toward-blanking).
#[cfg(target_os = "linux")]
fn set_all_inactive(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<RuleId, bool>,
    rules: &[ActivityRule],
) {
    for r in rules {
        publish(ctl, last_sent, &r.rule, false);
    }
}

/// Send a `SetInhibited` only when the value changed for this rule, and record
/// the new value **only on a successful send** so a dropped edge is retried on
/// the next poll rather than silently lost.
#[cfg(target_os = "linux")]
fn publish(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<RuleId, bool>,
    rule: &RuleId,
    inhibited: bool,
) {
    if last_sent.get(rule) == Some(&inhibited) {
        return;
    }
    if ctl
        .try_send(ControlMsg::SetInhibited {
            rule: Some(rule.clone()),
            inhibited,
        })
        .is_ok()
    {
        last_sent.insert(rule.clone(), inhibited);
    }
}

/// Query `GetSessionIdleTime` (raw units) from the screensaver service.
#[cfg(target_os = "linux")]
async fn get_idle_raw(conn: &zbus::Connection) -> zbus::Result<u64> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetSessionIdleTime",
            &(),
        )
        .await?;
    let idle: u32 = reply.body().deserialize()?;
    Ok(u64::from(idle))
}

/// Sleep for `dur` or return `true` if cancellation fired first.
#[cfg(target_os = "linux")]
async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(dur) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolvedUnit, UnitDetector, classify_delta, within_25};
    use std::time::Duration;

    const POLL: Duration = Duration::from_secs(5);

    fn detect(samples: &[u64]) -> Option<ResolvedUnit> {
        let mut d = UnitDetector::new(POLL, None);
        let mut last = None;
        for &s in samples {
            last = d.observe(s);
        }
        last
    }

    #[test]
    fn active_user_jitter_stays_undetermined() {
        // 10→13 @5s: delta 3 is near neither 5000 (ms) nor 5 (s).
        assert_eq!(classify_delta(3, POLL), None);
        assert_eq!(detect(&[10, 13]), None);
    }

    #[test]
    fn locks_milliseconds_after_two_corroborating_samples() {
        // Two idle-growth deltas of ~5000.
        assert_eq!(detect(&[0, 5_000, 10_000]), Some(ResolvedUnit::Ms));
    }

    #[test]
    fn locks_seconds_after_two_corroborating_samples() {
        // Two idle-growth deltas of ~5.
        assert_eq!(detect(&[0, 5, 10]), Some(ResolvedUnit::Secs));
    }

    #[test]
    fn conflicting_streak_resets_and_does_not_lock() {
        // delta 5000 (ms) then delta 5 (s) — conflicting single samples.
        assert_eq!(detect(&[0, 5_000, 5_005]), None);
    }

    #[test]
    fn single_sample_never_locks() {
        assert_eq!(detect(&[0, 5_000]), None);
    }

    #[test]
    fn forced_unit_locks_immediately() {
        let mut d = UnitDetector::new(POLL, Some(ResolvedUnit::Ms));
        assert_eq!(d.observe(42), Some(ResolvedUnit::Ms));
    }

    #[test]
    fn classify_delta_picks_exactly_one_unit() {
        assert_eq!(classify_delta(5_000, POLL), Some(ResolvedUnit::Ms));
        assert_eq!(classify_delta(5, POLL), Some(ResolvedUnit::Secs));
        assert_eq!(classify_delta(100, POLL), None);
    }

    #[test]
    fn to_ms_scales_seconds() {
        assert_eq!(ResolvedUnit::Ms.to_ms(1500), 1500);
        assert_eq!(ResolvedUnit::Secs.to_ms(3), 3000);
    }

    #[test]
    fn within_25_bounds() {
        assert!(within_25(5000, 5000));
        assert!(within_25(3750, 5000));
        assert!(within_25(6250, 5000));
        assert!(!within_25(3000, 5000));
        assert!(!within_25(7000, 5000));
        assert!(!within_25(5, 0));
    }
}
