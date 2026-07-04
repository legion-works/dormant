//! Shared backoff and emit-unavailable helpers for sensor sources.
//!
//! Both [`crate::mqtt::MqttSource`] and [`crate::ha_ws::HaWsSource`] use the
//! same capped-exponential-backoff-with-jitter strategy and the same pattern
//! for emitting [`SensorState::Unavailable`] for all owned sensors.  This
//! module factors out the duplication.

use std::time::Duration;

use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use tokio::sync::mpsc;

// ── Backoff ───────────────────────────────────────────────────────────────────

/// Compute the next backoff duration with capped exponential growth and ±20%
/// jitter.
///
/// - `current`: the current backoff duration.
/// - `min`: minimum backoff (clamped below).
/// - `max`: maximum backoff (clamped above).
/// - `jitter_fraction`: jitter range as a fraction of the next value (e.g.
///   `0.20` for ±20%).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
#[must_use]
pub fn next_backoff(
    current: Duration,
    min: Duration,
    max: Duration,
    jitter_fraction: f64,
) -> Duration {
    let next = current.mul_f64(2.0).min(max);
    if jitter_fraction <= 0.0 {
        return next.max(min).min(max);
    }
    let jitter_range_ms = next.mul_f64(jitter_fraction).as_millis();
    if jitter_range_ms == 0 {
        return next.max(min).min(max);
    }
    let offset_ms = ((fastrand::f64() * 2.0 - 1.0) * jitter_range_ms as f64) as i64;
    let result = if offset_ms >= 0 {
        next.saturating_add(Duration::from_millis(offset_ms as u64))
    } else {
        next.saturating_sub(Duration::from_millis((-offset_ms) as u64))
    };
    result.max(min).min(max)
}

// ── Emit unavailable ─────────────────────────────────────────────────────────

/// Emit [`SensorState::Unavailable`] for every sensor in `ids`.
///
/// Returns immediately (without emitting) if the receiver has been dropped.
pub async fn emit_unavailable_all(ids: &[SensorId], tx: &mpsc::Sender<PresenceEvent>) {
    let now = Timestamp::now();
    for sensor_id in ids {
        let event = PresenceEvent::new(sensor_id.clone(), SensorState::Unavailable, now);
        if tx.send(event).await.is_err() {
            return;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    #[test]
    fn backoff_stays_within_bounds() {
        let min = Duration::from_millis(250);
        let max = Duration::from_secs(30);
        let mut b = min;
        for _ in 0..20 {
            b = next_backoff(b, min, max, 0.20);
            assert!(
                b >= min && b <= max,
                "backoff {b:?} out of bounds [{min:?}, {max:?}]",
            );
        }
    }

    #[test]
    fn backoff_eventually_caps() {
        let min = Duration::from_millis(250);
        let max = Duration::from_secs(30);
        let mut b = min;
        for _ in 0..10 {
            b = next_backoff(b, min, max, 0.20);
        }
        assert!(
            b >= Duration::from_secs(20),
            "backoff {b:?} should be near cap"
        );
    }

    #[test]
    fn backoff_zero_jitter_no_panic() {
        let min = Duration::from_millis(250);
        let max = Duration::from_secs(30);
        let b = next_backoff(min, min, max, 0.0);
        assert!(b >= min && b <= max);
    }
}
