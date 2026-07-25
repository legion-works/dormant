//! Replaces the activity-claim policy evaluator: detect a genuine local
//! activity edge and pull the shared display here via direct DDC writes.
//!
//! The edge detector and filtered-input precedence are ported from
//! [`crate::activity_claim_evaluator::detect_activity_edge`]; all
//! claim-policy branches (`OwnerIdle`, `Armed`, `IdleQuery`/`IdleReport`, peers)
//! are gone.
//!
//! ## Split (`audio_policy` / `wear_tracker` house pattern)
//!
//! - `detect_activity_edge` — pure edge-detection logic, ported from the
//!   claim evaluator.
//! - This module — async shell: watches `IdleObservationRx` and
//!   `FilteredActivityRx`, drives `DirectSwitchHandle::pull` on a real edge
//!   after the configured `arm_after` grace window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dormant_core::types::DisplayId;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::direct_switch::{DirectSwitchHandle, SwitchReason};
use crate::filtered_activity::{FilteredActivity, FilteredActivityRx};
use crate::idle_observation::{IdleObservation, IdleObservationRx};

/// Production monotonic clock — delegates to the tokio runtime so
/// paused-time tests (`#[tokio::test(start_paused = true)]`) can
/// control time via `tokio::time::advance()`.
#[allow(dead_code)]
fn production_clock() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Dependencies for the activity-follow task.
pub struct ActivityFollowDeps {
    /// Daemon-lifetime idle-observation channel.
    pub idle_rx: IdleObservationRx,
    /// Direct-switch handle for local input-source writes.
    ///
    /// `None` only during tests with `pull_recorder` set; at runtime
    /// this is always populated.
    pub direct_switch: Option<DirectSwitchHandle>,
    /// Shared displays that are not locally owned — the set the loop pulls on.
    pub display_ids: Arc<[DisplayId]>,
    /// Grace window after an activity edge before the pull commits.
    pub arm_after: Duration,
    /// Daemon-lifetime cancellation token.
    pub cancel: CancellationToken,
    /// Test seam: when set, the loop sends each `DisplayId` here after the
    /// `arm_after` window instead of calling `DirectSwitchHandle::pull`.
    pub pull_recorder: Option<tokio::sync::mpsc::UnboundedSender<DisplayId>>,
    /// Replaceable monotonic clock — delegates to
    /// `tokio::time::Instant::now().into_std()` so paused-time tests can
    /// control the timeline via `advance()`.
    pub clock: fn() -> Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivityUpdate {
    Stock,
    Filtered,
}

/// Detect a genuine local activity edge from idle observations.
///
/// Ported from [`crate::activity_claim_evaluator::detect_activity_edge`].
/// Filtered input is the canonical edge authority when available; when the
/// filtered source dies, the function falls back to raw stock-idle deltas
/// after recording the current stock timestamp as a new baseline — the first
/// stock update after fallback is never an edge by itself.
fn detect_activity_edge(
    update: ActivityUpdate,
    stock: &IdleObservation,
    filtered: &FilteredActivity,
    last_stock_activity: &mut Option<Instant>,
    last_filtered_edge_seq: &mut u64,
    filtered_was_available: &mut bool,
) -> bool {
    if filtered.available {
        *filtered_was_available = true;
        if update == ActivityUpdate::Filtered && filtered.edge_seq > *last_filtered_edge_seq {
            *last_filtered_edge_seq = filtered.edge_seq;
            return true;
        }
        return false;
    }

    // Filtered source just became unavailable — reset the stock baseline
    // so the first stock update after fallback is not a spurious edge.
    if std::mem::take(filtered_was_available) {
        *last_stock_activity = stock.last_activity;
        return false;
    }

    match (*last_stock_activity, stock.last_activity) {
        (None, Some(activity)) => {
            *last_stock_activity = Some(activity);
            true
        }
        (Some(previous), Some(activity)) if activity > previous => {
            *last_stock_activity = Some(activity);
            true
        }
        (Some(_), Some(_)) => false,
        (_, None) => {
            *last_stock_activity = None;
            false
        }
    }
}

/// Spawn the activity-follow task with filtered activity as the canonical
/// edge authority.
#[must_use]
pub fn spawn(deps: ActivityFollowDeps, filtered_rx: FilteredActivityRx) -> JoinHandle<()> {
    tokio::spawn(async move {
        activity_follow_loop(deps, filtered_rx).await;
    })
}

async fn activity_follow_loop(deps: ActivityFollowDeps, mut filtered_rx: FilteredActivityRx) {
    let mut idle_rx = deps.idle_rx;
    let mut last_stock_activity: Option<Instant> = None;
    let mut last_filtered_edge_seq = 0u64;
    let mut filtered_was_available = false;
    // When set, a real edge was detected; the pull commits once `arm_after`
    // has elapsed. The cooldown is enforced inside DirectSwitchHandle::pull
    // (SwitchReason::Activity path) — no private second clock.
    let mut edge_time: Option<Instant> = None;

    loop {
        let (observation, filtered, update) = tokio::select! {
            () = deps.cancel.cancelled() => return,
            changed = idle_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                // Only mark the channel that changed as seen;
                // the other channel peeked with borrow() so its
                // pending change survives to the next iteration.
                let obs = idle_rx.borrow_and_update().clone();
                let filt = filtered_rx.borrow().clone();
                (obs, filt, ActivityUpdate::Stock)
            }
            changed = filtered_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let obs = idle_rx.borrow().clone();
                let filt = filtered_rx.borrow_and_update().clone();
                (obs, filt, ActivityUpdate::Filtered)
            }
        };

        let edge = detect_activity_edge(
            update,
            &observation,
            &filtered,
            &mut last_stock_activity,
            &mut last_filtered_edge_seq,
            &mut filtered_was_available,
        );

        if edge {
            edge_time = Some((deps.clock)());
        }

        if let Some(et) = edge_time
            && et + deps.arm_after <= (deps.clock)()
        {
            if let Some(recorder) = &deps.pull_recorder {
                for display_id in &*deps.display_ids {
                    let _ = recorder.send(display_id.clone());
                }
            } else if let Some(ref direct_switch) = deps.direct_switch {
                for display_id in &*deps.display_ids {
                    let _ = direct_switch
                        .pull(display_id.clone(), SwitchReason::Activity)
                        .await;
                }
            }
            edge_time = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use dormant_core::types::DisplayId;
    use tokio_util::sync::CancellationToken;

    use crate::filtered_activity::{FilteredActivity, filtered_activity_channel};
    use crate::idle_observation::{IdleObservation, idle_observation_channel};

    use super::{
        ActivityFollowDeps, ActivityUpdate, detect_activity_edge, production_clock, spawn,
    };

    // ── detect_activity_edge unit tests ────────────────────────────────────

    #[test]
    fn filtered_startup_timestamp_is_not_an_activity_edge() {
        let now = Instant::now();
        let stock = IdleObservation {
            last_activity: Some(now),
            observed_at: now,
            available: true,
        };
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 0,
        };
        let mut last_stock = None;
        let mut last_edge = 0;
        let mut was_filtered = false;

        assert!(!detect_activity_edge(
            ActivityUpdate::Filtered,
            &stock,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
    }

    #[test]
    fn filtered_edge_sequence_is_the_edge_authority() {
        let now = Instant::now();
        let stock = IdleObservation {
            last_activity: Some(now),
            observed_at: now,
            available: true,
        };
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 1,
        };
        let mut last_stock = None;
        let mut last_edge = 0;
        let mut was_filtered = true;

        assert!(detect_activity_edge(
            ActivityUpdate::Filtered,
            &stock,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        assert_eq!(last_edge, 1);
    }

    #[test]
    fn first_stock_timestamp_after_fallback_is_only_a_baseline() {
        let now = Instant::now();
        let later = now.checked_add(Duration::from_secs(1)).unwrap();
        let stock = IdleObservation {
            last_activity: Some(later),
            observed_at: later,
            available: true,
        };
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: later,
            available: false,
            edge_seq: 1,
        };
        let mut last_stock = Some(now);
        let mut last_edge = 1;
        let mut was_filtered = true;

        assert!(!detect_activity_edge(
            ActivityUpdate::Stock,
            &stock,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        assert_eq!(last_stock, Some(later));
    }

    /// Continuous filtered input (same `edge_seq`) must NOT produce an edge.
    #[test]
    fn continuous_filtered_input_no_repeated_edge() {
        let now = Instant::now();
        let stock = IdleObservation {
            last_activity: Some(now),
            observed_at: now,
            available: true,
        };
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 3,
        };
        let mut last_stock = None;
        let mut last_edge = 3; // already at edge_seq 3
        let mut was_filtered = true;

        // Same edge_seq — no new edge.
        assert!(!detect_activity_edge(
            ActivityUpdate::Filtered,
            &stock,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        // edge_seq must not advance.
        assert_eq!(last_edge, 3);
    }

    /// An ignored device (`edge_seq` unchanged) must NOT produce an edge.
    #[test]
    fn ignored_device_no_edge() {
        let now = Instant::now();
        let stock = IdleObservation {
            last_activity: Some(now),
            observed_at: now,
            available: true,
        };
        // Simulate: filtered source is available but the activity came from
        // an ignored device — edge_seq did not advance.
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 7,
        };
        let mut last_stock = Some(now);
        let mut last_edge = 7; // already at edge_seq 7
        let mut was_filtered = true;

        assert!(!detect_activity_edge(
            ActivityUpdate::Filtered,
            &stock,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        assert_eq!(last_edge, 7);
    }

    /// When filtered source is available and stock fires (not filtered),
    /// no edge is produced — filtered is canonical.
    #[test]
    fn stock_update_ignored_when_filtered_available() {
        let now = Instant::now();
        let later = now.checked_add(Duration::from_secs(5)).unwrap();
        let stock = IdleObservation {
            last_activity: Some(later),
            observed_at: later,
            available: true,
        };
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: later,
            available: true,
            edge_seq: 4,
        };
        let mut last_stock = Some(now);
        let mut last_edge = 4;
        let mut was_filtered = true;

        // Stock update while filtered is available — must NOT produce an edge.
        assert!(!detect_activity_edge(
            ActivityUpdate::Stock,
            &stock,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        // last_stock must remain at the old baseline (filtered was never lost).
        assert_eq!(last_stock, Some(now));
    }

    /// Filtered source loss: the first stock update resets the baseline, no
    /// edge. The second stock update with a newer timestamp fires an edge.
    #[test]
    fn filtered_loss_then_stock_recovery_produces_edge() {
        let now = Instant::now();
        let later = now.checked_add(Duration::from_secs(1)).unwrap();
        let later2 = later.checked_add(Duration::from_secs(1)).unwrap();

        // Filtered just became unavailable.
        let filtered = FilteredActivity {
            last_activity: Some(now),
            observed_at: later,
            available: false,
            edge_seq: 5,
        };
        let mut last_stock = Some(now);
        let mut last_edge = 5;
        let mut was_filtered = true;

        // First stock after loss — baseline reset, no edge.
        let stock1 = IdleObservation {
            last_activity: Some(later),
            observed_at: later,
            available: true,
        };
        assert!(!detect_activity_edge(
            ActivityUpdate::Stock,
            &stock1,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        assert_eq!(last_stock, Some(later));
        assert!(!was_filtered);

        // Second stock with newer timestamp — edge fires.
        let stock2 = IdleObservation {
            last_activity: Some(later2),
            observed_at: later2,
            available: true,
        };
        assert!(detect_activity_edge(
            ActivityUpdate::Stock,
            &stock2,
            &filtered,
            &mut last_stock,
            &mut last_edge,
            &mut was_filtered,
        ));
        assert_eq!(last_stock, Some(later2));
    }

    // ── Loop integration tests ─────────────────────────────────────────────

    /// Helper: spawn the loop with a recording pull sink and no direct switch.
    fn spawn_for_test(
        idle_rx: crate::idle_observation::IdleObservationRx,
        filtered_rx: crate::filtered_activity::FilteredActivityRx,
        display_ids: Arc<[DisplayId]>,
        arm_after: Duration,
        pull_recorder: tokio::sync::mpsc::UnboundedSender<DisplayId>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let deps = ActivityFollowDeps {
            idle_rx,
            direct_switch: None,
            display_ids,
            arm_after,
            cancel,
            pull_recorder: Some(pull_recorder),
            clock: production_clock,
        };
        spawn(deps, filtered_rx)
    }

    #[tokio::test(start_paused = true)]
    async fn idle_to_activity_pulls_display() {
        let (idle_tx, idle_rx) = idle_observation_channel();
        let (_filtered_tx, filtered_rx) = filtered_activity_channel();
        let (pull_tx, mut pull_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let display_ids: Arc<[DisplayId]> = Arc::from([DisplayId("shared".into())]);

        let handle = spawn_for_test(
            idle_rx,
            filtered_rx,
            display_ids,
            Duration::ZERO,
            pull_tx,
            cancel.clone(),
        );

        // First observation with activity — the None→Some transition fires an
        // edge, and with arm_after=0 the pull commits immediately.
        let t0 = Instant::now();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();

        // Yield once so the spawned loop can process the edge and send the pull.
        tokio::task::yield_now().await;
        let recorded = pull_rx.try_recv().expect("expected a pull after yield");
        assert_eq!(recorded, DisplayId("shared".into()));

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_activity_no_second_pull() {
        let (idle_tx, idle_rx) = idle_observation_channel();
        let (_filtered_tx, filtered_rx) = filtered_activity_channel();
        let (pull_tx, mut pull_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let display_ids: Arc<[DisplayId]> = Arc::from([DisplayId("shared".into())]);

        let handle = spawn_for_test(
            idle_rx,
            filtered_rx,
            display_ids,
            Duration::ZERO,
            pull_tx,
            cancel.clone(),
        );

        // First observation triggers edge → pull.
        let t0 = Instant::now();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();
        tokio::task::yield_now().await;
        let first = pull_rx.try_recv().expect("expected first pull");
        assert_eq!(first, DisplayId("shared".into()));

        // Second observation with SAME timestamp — no edge, no pull.
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();

        tokio::task::yield_now().await;
        assert!(
            pull_rx.try_recv().is_err(),
            "continuous activity with same timestamp must not produce a second pull"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn ignored_device_no_pull() {
        let (idle_tx, idle_rx) = idle_observation_channel();
        let (filtered_tx, filtered_rx) = filtered_activity_channel();
        let (pull_tx, mut pull_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let display_ids: Arc<[DisplayId]> = Arc::from([DisplayId("shared".into())]);

        let handle = spawn_for_test(
            idle_rx,
            filtered_rx,
            display_ids,
            Duration::ZERO,
            pull_tx,
            cancel.clone(),
        );

        // Seed idle baseline — first idle observation triggers an edge from
        // the None→Some transition.
        let t0 = Instant::now();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();
        tokio::task::yield_now().await;
        while pull_rx.try_recv().is_ok() {} // drain initial edge pull

        // Make filtered available with edge_seq=1.  The edge_seq advance
        // (0→1) is a real edge — drain the resulting pull.
        filtered_tx
            .send(FilteredActivity {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
                edge_seq: 1,
            })
            .unwrap();
        tokio::task::yield_now().await;
        while pull_rx.try_recv().is_ok() {} // drain filtered-authority edge

        // Now filtered is authoritative.  Send a filtered update with the
        // SAME edge_seq — simulates an ignored device whose activity did not
        // advance the edge counter.
        filtered_tx
            .send(FilteredActivity {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
                edge_seq: 1, // unchanged
            })
            .unwrap();

        // Simultaneously send a stock observation with a newer timestamp.
        // Since filtered is available, stock edges are suppressed.
        let t1 = t0.checked_add(Duration::from_secs(10)).unwrap();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t1),
                observed_at: t1,
                available: true,
            })
            .unwrap();

        tokio::task::yield_now().await;
        assert!(
            pull_rx.try_recv().is_err(),
            "ignored-device activity must not produce a pull"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn filtered_fallback_to_stock_produces_pull() {
        let (idle_tx, idle_rx) = idle_observation_channel();
        let (filtered_tx, filtered_rx) = filtered_activity_channel();
        let (pull_tx, mut pull_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let display_ids: Arc<[DisplayId]> = Arc::from([DisplayId("shared".into())]);

        let handle = spawn_for_test(
            idle_rx,
            filtered_rx,
            display_ids,
            Duration::ZERO,
            pull_tx,
            cancel.clone(),
        );

        // Establish baseline: stock observation with idle timestamp.
        let t0 = Instant::now();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();
        tokio::task::yield_now().await;
        while pull_rx.try_recv().is_ok() {} // drain initial edge pull

        // Make filtered available — establishes filtered authority.
        filtered_tx
            .send(FilteredActivity {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
                edge_seq: 1,
            })
            .unwrap();
        tokio::task::yield_now().await;
        while pull_rx.try_recv().is_ok() {} // drain authority-edge pull

        // Filtered becomes unavailable — the fallback begins.
        filtered_tx
            .send(FilteredActivity {
                last_activity: Some(t0),
                observed_at: t0,
                available: false,
                edge_seq: 1,
            })
            .unwrap();
        // The loop resets the stock baseline — no pull yet.
        tokio::task::yield_now().await;
        assert!(pull_rx.try_recv().is_err(), "fallback reset must not pull");

        // Now send a stock observation with a newer activity timestamp.
        // Since filtered is unavailable AND the baseline was reset, this
        // fires an edge.
        let t1 = t0.checked_add(Duration::from_secs(5)).unwrap();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t1),
                observed_at: t1,
                available: true,
            })
            .unwrap();

        tokio::task::yield_now().await;
        let recorded = pull_rx
            .try_recv()
            .expect("fallback must produce a pull after stock edge");
        assert_eq!(recorded, DisplayId("shared".into()));

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn arm_after_delays_pull() {
        let (idle_tx, idle_rx) = idle_observation_channel();
        let (_filtered_tx, filtered_rx) = filtered_activity_channel();
        let (pull_tx, mut pull_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let display_ids: Arc<[DisplayId]> = Arc::from([DisplayId("shared".into())]);

        let arm_after = Duration::from_millis(100);
        let handle = spawn_for_test(
            idle_rx,
            filtered_rx,
            display_ids,
            arm_after,
            pull_tx,
            cancel.clone(),
        );

        // Send observation that triggers an edge.
        let t0 = Instant::now();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();

        // Let the loop process the edge.  arm_after has not elapsed yet,
        // so no pull should be recorded.
        tokio::task::yield_now().await;
        assert!(
            pull_rx.try_recv().is_err(),
            "pull must not fire before arm_after expires"
        );

        // Advance the paused clock past arm_after.
        tokio::time::advance(arm_after).await;

        // Send a second observation (same timestamp, no new edge) to wake
        // the loop and trigger the arm-expiry check.
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();

        tokio::task::yield_now().await;
        let recorded = pull_rx
            .try_recv()
            .expect("pull must fire after arm_after expires");
        assert_eq!(recorded, DisplayId("shared".into()));

        cancel.cancel();
        let _ = handle.await;
    }

    /// When the filtered source is available, a stock observation that fires
    /// (with a newer timestamp) must NOT produce a pull — filtered is canonical.
    #[tokio::test(start_paused = true)]
    async fn stock_edge_blocked_while_filtered_available() {
        let (idle_tx, idle_rx) = idle_observation_channel();
        let (filtered_tx, filtered_rx) = filtered_activity_channel();
        let (pull_tx, mut pull_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let display_ids: Arc<[DisplayId]> = Arc::from([DisplayId("shared".into())]);

        let handle = spawn_for_test(
            idle_rx,
            filtered_rx,
            display_ids,
            Duration::ZERO,
            pull_tx,
            cancel.clone(),
        );

        // Baseline stock observation.
        let t0 = Instant::now();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
            })
            .unwrap();
        tokio::task::yield_now().await;
        while pull_rx.try_recv().is_ok() {} // drain initial edge pull

        // Make filtered authoritative with edge_seq=2.
        filtered_tx
            .send(FilteredActivity {
                last_activity: Some(t0),
                observed_at: t0,
                available: true,
                edge_seq: 2,
            })
            .unwrap();
        tokio::task::yield_now().await;
        while pull_rx.try_recv().is_ok() {} // drain authority-edge pull

        // Now send a stock observation with a newer timestamp. Filtered is
        // still available → stock edges are suppressed.
        let t1 = t0.checked_add(Duration::from_secs(10)).unwrap();
        idle_tx
            .send(IdleObservation {
                last_activity: Some(t1),
                observed_at: t1,
                available: true,
            })
            .unwrap();

        tokio::task::yield_now().await;
        assert!(
            pull_rx.try_recv().is_err(),
            "stock edge must be suppressed while filtered is available"
        );

        cancel.cancel();
        let _ = handle.await;
    }
}
