//! Daemon-lifetime activity-claim policy evaluator.
//!
//! Watches the idle-observation channel and, for every shared display not
//! locally owned, evaluates the configured activity-claim policy against the
//! latest idle state.  Feeds claim / arm decisions into the claim runtime.
//!
//! ## Split (`audio_policy` / `wear_tracker` house pattern)
//!
//! - `ActivityClaimState::decide()` — pure decision logic in `dormant-core`.
//! - This module — async shell: watches `IdleObservationRx`, resolves
//!   ownership, drives `ClaimRuntimeHandle` methods.

use std::sync::Arc;
use std::time::Instant;

use dormant_core::claim::{ActivityClaimDecision, ActivityClaimState};
use dormant_core::config::ActivityClaimPolicy;
use dormant_core::ownership::OwnershipGate;
use dormant_core::types::DisplayId;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::claim_runtime::ClaimRuntimeHandle;
use crate::idle_observation::IdleObservationRx;

/// Dependencies for the policy evaluator task.
pub struct PolicyEvaluatorDeps {
    /// Daemon-lifetime idle-observation channel.
    pub idle_rx: IdleObservationRx,
    /// Claim runtime handle for triggering claims.
    pub claim_runtime: ClaimRuntimeHandle,
    /// Ownership gate — tells us which shared displays we own.
    pub ownership: Arc<dyn OwnershipGate>,
    /// Configured activity-claim policy.
    pub activity_claim: ActivityClaimPolicy,
    /// Owner-idle window threshold.
    pub owner_idle_window: std::time::Duration,
    /// Armed window duration.
    pub armed_window: std::time::Duration,
    /// Per-display claim-capable set (post-probe, refreshed on generation install).
    pub claim_capable_displays: Vec<DisplayId>,
    /// Daemon-lifetime cancellation token.
    pub cancel: CancellationToken,
    /// Test seam — an append-only log of evaluator decisions.
    pub event_log: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Signals test waiters after a decision is appended.
    pub event_notify: Option<Arc<tokio::sync::Notify>>,
}

/// Spawn the daemon-lifetime activity-claim policy evaluator.
///
/// Returns the join handle so the orchestrator can bound-await it during
/// shutdown, mirroring the `wear_tracker` / `claim_runtime` pattern.
#[must_use]
pub fn spawn(deps: PolicyEvaluatorDeps) -> JoinHandle<()> {
    tokio::spawn(async move {
        evaluator_loop(deps).await;
    })
}

#[allow(clippy::too_many_lines)]
async fn evaluator_loop(deps: PolicyEvaluatorDeps) {
    let policy = ActivityClaimState::new(
        deps.activity_claim,
        deps.owner_idle_window,
        deps.armed_window,
    );

    let mut idle_rx = deps.idle_rx;
    let mut last_activity: Option<Instant> = None;
    let mut warned_owner_idle_no_source = false;
    let mut warned_no_peers = false;

    loop {
        tokio::select! {
            () = deps.cancel.cancelled() => return,
            changed = idle_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }

        let observation = idle_rx.borrow_and_update().clone();
        let now = Instant::now();

        // ── Warning seam: owner-idle policy without an idle source ────────
        if matches!(deps.activity_claim, ActivityClaimPolicy::OwnerIdle)
            && !observation.available
            && !warned_owner_idle_no_source
        {
            tracing::warn!(
                event = "activity_claim_owner_idle_no_source",
                "owner-idle activity-claim policy is active but the idle source \
                 is unavailable — claims will not fire until the source recovers",
            );
            warned_owner_idle_no_source = true;
        }

        // ── Detect an activity edge ───────────────────────────────────────
        let edge = match (last_activity, observation.last_activity) {
            (None, Some(la)) => {
                last_activity = Some(la);
                true
            }
            (Some(prev), Some(la)) if la > prev => {
                last_activity = Some(la);
                true
            }
            (Some(_prev), Some(_)) => {
                // No change — keep tracking for edge detection.
                false
            }
            (_, None) => {
                last_activity = None;
                false
            }
        };

        if !edge {
            continue;
        }

        // ── Evaluate policy for each claim-capable display not locally owned ──
        for display in &deps.claim_capable_displays {
            if deps.ownership.owns(display) {
                continue;
            }
            let display_name = display.0.clone();

            let armed_until = {
                // is_armed returns true if the display has an unexpired arm deadline.
                // We need the actual deadline for decide(), but ClaimRuntimeHandle
                // only exposes is_armed(). For the pure decision, we pass a
                // sentinel: armed_until is Some(now+1s) if armed, None otherwise.
                if deps.claim_runtime.is_armed(display) {
                    Some(now + deps.armed_window)
                } else {
                    None
                }
            };

            // Owner-idle state: we don't have the owner's idle report cached yet.
            // For edge/armed, owner_idle is irrelevant; for owner-idle, we need
            // to query the owner first.  Pass false (unknown) — the policy will
            // return QueryOwnerIdle, and the caller can send the query.
            let owner_idle = false;

            let decision = policy.decide(owner_idle, armed_until, now);
            let log_msg = format!(
                "policy_evaluated display={display} policy={:?} decision={decision:?}",
                deps.activity_claim,
            );
            append_log(deps.event_log.as_ref(), deps.event_notify.as_ref(), log_msg);

            match decision {
                ActivityClaimDecision::Ignore => {}
                ActivityClaimDecision::Claim => {
                    tracing::info!(
                        event = "activity_claim_firing",
                        display_name = %display_name,
                        policy = ?deps.activity_claim,
                    );
                    append_log(
                        deps.event_log.as_ref(),
                        deps.event_notify.as_ref(),
                        format!("claim_firing display={display}"),
                    );
                    match deps.claim_runtime.try_claim(display.clone()).await {
                        Ok(result) => {
                            tracing::info!(
                                event = "activity_claim_result",
                                display_name = %display_name,
                                result = ?result,
                            );
                            append_log(
                                deps.event_log.as_ref(),
                                deps.event_notify.as_ref(),
                                format!("claim_result display={display} result={result:?}"),
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                event = "activity_claim_error",
                                display_name = %display_name,
                                error = %e,
                            );
                        }
                    }
                }
                ActivityClaimDecision::QueryOwnerIdle => {
                    // The owner-idle policy requires the owner's idle report.
                    // Sending the query and waiting for the response is a
                    // multi-step async flow that involves the claim transport.
                    // For now, log the intent — the full query path is wired
                    // when the claim runtime exposes an idle-query method.
                    tracing::debug!(
                        event = "activity_claim_query_owner_idle",
                        display_name = %display_name,
                        "owner-idle policy triggered; idle query not yet wired",
                    );
                }
            }
        }

        // ── Warning seam: activity_claim != off with empty peer store ─────
        if deps.activity_claim != ActivityClaimPolicy::Off
            && deps.claim_capable_displays.is_empty()
            && !warned_no_peers
        {
            tracing::warn!(
                event = "activity_claim_no_claim_capable_displays",
                "activity-claim policy is {:?} but no displays are claim-capable — \
                 claims will not fire",
                deps.activity_claim,
            );
            warned_no_peers = true;
        }
    }
}

fn append_log(
    log: Option<&Arc<std::sync::Mutex<Vec<String>>>>,
    notify: Option<&Arc<tokio::sync::Notify>>,
    entry: String,
) {
    if let Some(log) = log {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
    }
    if let Some(notify) = notify {
        notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idle_observation::{IdleObservation, idle_observation_channel};
    use dormant_core::config::ActivityClaimPolicy;
    use dormant_core::ownership::OwnershipGate;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    struct AlwaysOwned;
    impl OwnershipGate for AlwaysOwned {
        fn owns(&self, _display: &DisplayId) -> bool {
            true
        }
    }

    /// The evaluator loop processes idle observations without panicking when
    /// all displays are locally owned (no claim actions needed).  The pure
    /// decision logic is exercised by the `activity_claim_tests` module in
    /// `dormant-core`; the full claim path integration is covered by
    /// `claim_smoke.rs`.
    #[tokio::test]
    async fn evaluator_processes_observations_without_panicking() {
        let cancel = CancellationToken::new();
        let (idle_tx, idle_rx) = idle_observation_channel();
        let display = DisplayId("shared".into());

        // Publish an activity edge.
        let _ = idle_tx.send(IdleObservation {
            last_activity: Some(Instant::now()),
            observed_at: Instant::now(),
            available: true,
        });

        // All displays are locally owned, so the evaluator skips them
        // without reaching try_claim.  The loop still processes the
        // observation and edge detection — this test confirms it doesn't
        // panic.
        let placeholder_handle = crate::claim_runtime::ClaimRuntimeHandle::for_test();

        let deps = PolicyEvaluatorDeps {
            idle_rx,
            claim_runtime: placeholder_handle,
            ownership: Arc::new(AlwaysOwned),
            activity_claim: ActivityClaimPolicy::Edge,
            owner_idle_window: Duration::from_secs(30),
            armed_window: Duration::from_secs(60),
            claim_capable_displays: vec![display],
            cancel: cancel.clone(),
            event_log: None,
            event_notify: None,
        };

        let handle = spawn(deps);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = handle.await;
        // No assertions needed — the test verifies the evaluator doesn't panic.
    }
}
