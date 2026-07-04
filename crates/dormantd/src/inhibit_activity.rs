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

/// Decide the unit from two consecutive raw samples taken `poll` apart while
/// the user is (presumably) idle: a still-idle session accumulates roughly
/// `poll` of idle time between polls, so the delta lands near the poll
/// interval expressed in whichever unit the service uses.
///
/// Returns `None` when the samples are inconclusive (e.g. the user was active
/// and idle reset to ~0).
fn decide_unit(prev_raw: u64, curr_raw: u64, poll: Duration) -> Option<ResolvedUnit> {
    let delta = curr_raw.saturating_sub(prev_raw);
    if delta == 0 {
        return None;
    }
    let ms = u64::try_from(poll.as_millis()).unwrap_or(u64::MAX);
    let secs = poll.as_secs();
    if near(delta, ms) {
        Some(ResolvedUnit::Ms)
    } else if secs > 0 && near(delta, secs) {
        Some(ResolvedUnit::Secs)
    } else {
        None
    }
}

/// Whether `a` is within `[b/2, 3b/2]` (a ≈ b, tolerant).
fn near(a: u64, b: u64) -> bool {
    if b == 0 {
        return false;
    }
    let two_a = a.saturating_mul(2);
    two_a >= b && two_a <= b.saturating_mul(3)
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
    let mut resolved: Option<ResolvedUnit> = configured_unit(unit);
    let mut prev_raw: Option<u64> = None;
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

                if resolved.is_none() {
                    if let Some(prev) = prev_raw {
                        resolved = decide_unit(prev, raw, poll_interval);
                        if let Some(u) = resolved {
                            tracing::info!(event = "idle_unit_determined", unit = ?u);
                        }
                    }
                    prev_raw = Some(raw);
                }

                if let Some(unit) = resolved {
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
    use super::{ResolvedUnit, decide_unit, near};
    use std::time::Duration;

    #[test]
    fn detects_milliseconds_from_delta() {
        // Poll 5s apart, idle grew ~5000 → milliseconds.
        let u = decide_unit(1_000, 6_000, Duration::from_secs(5));
        assert_eq!(u, Some(ResolvedUnit::Ms));
    }

    #[test]
    fn detects_seconds_from_delta() {
        // Poll 5s apart, idle grew ~5 → seconds.
        let u = decide_unit(1, 6, Duration::from_secs(5));
        assert_eq!(u, Some(ResolvedUnit::Secs));
    }

    #[test]
    fn inconclusive_when_idle_reset() {
        // User was active — idle stayed ~0.
        assert_eq!(decide_unit(0, 0, Duration::from_secs(5)), None);
    }

    #[test]
    fn inconclusive_when_delta_matches_neither() {
        // Delta 100 is neither ≈5000 (ms) nor ≈5 (s).
        assert_eq!(decide_unit(0, 100, Duration::from_secs(5)), None);
    }

    #[test]
    fn to_ms_scales_seconds() {
        assert_eq!(ResolvedUnit::Ms.to_ms(1500), 1500);
        assert_eq!(ResolvedUnit::Secs.to_ms(3), 3000);
    }

    #[test]
    fn near_tolerates_half_to_one_and_a_half() {
        assert!(near(5000, 5000));
        assert!(near(2500, 5000));
        assert!(near(7500, 5000));
        assert!(!near(1000, 5000));
        assert!(!near(9000, 5000));
        assert!(!near(5, 0));
    }
}
