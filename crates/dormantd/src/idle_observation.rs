//! Daemon-lifetime idle-observation channel consumed by the activity-claim
//! policy.
//!
//! The stock idle source (DBus/Wayland/macOS) publishes real activity timestamps
//! and explicit availability into a watch channel. The activity-claim policy
//! evaluator reads these observations and decides whether to initiate a claim.
//!
//! ## Split (`audio_policy` / `wear_tracker` house pattern)
//!
//! - `IdleObservation` / `idle_observation_channel` — pure data structures.
//! - `idle_ms` — pure computation from an observation.
//! - The async policy-evaluator shell lives in the daemon's run loop.

use std::time::Instant;

/// One snapshot of the system idle state published by the stock source.
#[derive(Debug, Clone)]
pub struct IdleObservation {
    /// Last user-activity timestamp (monotonic), `None` when unavailable.
    pub last_activity: Option<Instant>,
    /// Monotonic time this sample was taken.
    pub observed_at: Instant,
    /// Whether the idle source is currently available and producing valid data.
    pub available: bool,
}

/// Reader side of a daemon-lifetime idle-observation channel.
pub type IdleObservationRx = tokio::sync::watch::Receiver<IdleObservation>;

/// Writer side of a daemon-lifetime idle-observation channel.
pub type IdleObservationTx = tokio::sync::watch::Sender<IdleObservation>;

/// Create a fresh idle-observation channel seeded with an unavailable state.
#[must_use]
pub fn idle_observation_channel() -> (IdleObservationTx, IdleObservationRx) {
    let initial = IdleObservation {
        last_activity: None,
        observed_at: Instant::now(),
        available: false,
    };
    tokio::sync::watch::channel(initial)
}

/// Compute the current idle duration in milliseconds from an observation.
///
/// Returns `None` when the source is unavailable, the last-activity timestamp
/// is unknown, or the observation is stale (`last_activity` lies in the future).
#[must_use]
pub fn idle_ms(observation: &IdleObservation, now: Instant) -> Option<u64> {
    if !observation.available {
        return None;
    }
    let last = observation.last_activity?;
    if last > now {
        return None;
    }
    let dur = now.saturating_duration_since(last);
    Some(u64::try_from(dur.as_millis()).unwrap_or(u64::MAX))
}
