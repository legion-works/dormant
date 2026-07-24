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
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    idlesrc: dormant_core::config::IdleSource,
    unit: IdleTimeUnit,
    macos_guard_cfg: crate::macos_idle::MacosIdleGuardConfig,
    idle_tx: Option<crate::idle_observation::IdleObservationTx>,
    input_filter: &dormant_core::config::InputFilterConfig,
    filtered_tx: crate::filtered_activity::FilteredActivityTx,
    ctl: mpsc::Sender<ControlMsg>,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(target_os = "linux")]
    if !rules.is_empty()
        && let Some(filtered) =
            crate::idle_source::create_filtered_source(input_filter, poll_interval)
    {
        let stock_rules = rules.clone();
        let stock_idle_tx = idle_tx.clone();
        let stock_factory: crate::filtered_activity::StockIdleFactory =
            std::sync::Arc::new(move || {
                crate::idle_source::create_source(
                    idlesrc,
                    stock_rules.clone(),
                    poll_interval,
                    unit,
                    macos_guard_cfg,
                    stock_idle_tx.clone(),
                )
            });
        let supervisor = crate::filtered_activity::InputAuthoritySupervisor::new(
            filtered,
            stock_factory,
            rules,
            poll_interval,
            idle_tx,
            filtered_tx,
            ctl,
        );
        return Some(tokio::spawn(supervisor.run(cancel)));
    }

    #[cfg(not(target_os = "linux"))]
    if !input_filter.ignore_devices.is_empty() {
        tracing::warn!(
            event = "input_filter_unavailable",
            reason = "input device filtering is not available on this platform",
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = filtered_tx;

    let source = crate::idle_source::create_source(
        idlesrc,
        rules,
        poll_interval,
        unit,
        macos_guard_cfg,
        idle_tx,
    )?;
    Some(tokio::spawn(async move { source.run(ctl, cancel).await }))
}
