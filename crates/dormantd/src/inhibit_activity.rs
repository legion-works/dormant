//! User-activity inhibitor.
//!
//! Selects an idle source (Wayland `ext_idle_notifier_v1` or `DBus`
//! `GetSessionIdleTime` poll) and publishes per-rule inhibition into the rules
//! engine via [`ControlMsg::SetInhibited`]. The source is chosen from
//! `daemon.idle_source` — `"auto"` (default) prefers Wayland when available,
//! `"wayland"` or `"dbus"` force one path.
//!
//! ## Fail-toward-normal-blanking
//!
//! If the idle source is unreachable or errors, the inhibitor treats the user
//! as **inactive** — a broken idle probe must never wedge displays awake; the
//! sensor/zone layer still guards actual presence.

use std::time::Duration;

use dormant_core::config::IdleTimeUnit;
use dormant_core::rules::ControlMsg;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// Re-export ActivityRule from idle_source (single source of truth).
pub use crate::idle_source::ActivityRule;

/// Spawn the activity-inhibitor poller.
///
/// Returns `None` (spawning nothing) when no rule declares `user-activity`.
#[must_use]
pub fn spawn(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    idlesrc: dormant_core::config::IdleSource,
    unit: IdleTimeUnit,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let source = crate::idle_source::create_source(idlesrc, rules, poll_interval, unit)?;
    Some(tokio::spawn(async move { source.run(ctl, cancel).await }))
}
