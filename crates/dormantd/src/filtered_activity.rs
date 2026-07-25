//! Platform-neutral filtered-input activity and authority supervision.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dormant_core::rules::ControlMsg;
use dormant_core::types::RuleId;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::idle_observation::{IdleObservation, IdleObservationTx};
use crate::idle_source::{self, ActivityRule, IdleSource};

/// One filtered-input snapshot shared by every activity consumer.
#[derive(Debug, Clone)]
pub struct FilteredActivity {
    /// Last accepted-device activity timestamp.
    pub last_activity: Option<Instant>,
    /// Monotonic time at which the source confirmed this snapshot.
    pub observed_at: Instant,
    /// Whether at least one accepted device is readable.
    pub available: bool,
    /// Monotonic accepted-activity edge sequence.
    pub edge_seq: u64,
}

impl FilteredActivity {
    /// Return an unavailable seed value for a new channel or failed source.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            last_activity: None,
            observed_at: Instant::now(),
            available: false,
            edge_seq: 0,
        }
    }

    /// Whether an accepted activity edge and its source heartbeat are fresh.
    #[must_use]
    pub fn is_recent(&self, now: Instant, maximum_age: Duration) -> bool {
        if !self.available || self.observed_at > now {
            return false;
        }
        let Some(last_activity) = self.last_activity else {
            return false;
        };
        last_activity <= now
            && now.saturating_duration_since(self.observed_at) <= maximum_age
            && now.saturating_duration_since(last_activity) <= maximum_age
    }
}

/// Reader side of the daemon-lifetime filtered-activity fan-out.
pub type FilteredActivityRx = watch::Receiver<FilteredActivity>;

/// Writer side of the daemon-lifetime filtered-activity fan-out.
pub type FilteredActivityTx = watch::Sender<FilteredActivity>;

/// Create a filtered-activity channel seeded unavailable.
#[must_use]
pub fn filtered_activity_channel() -> (FilteredActivityTx, FilteredActivityRx) {
    watch::channel(FilteredActivity::unavailable())
}

/// Case-insensitive device-name glob matcher.
#[derive(Debug, Clone)]
pub struct DeviceMatcher {
    patterns: Vec<Vec<char>>,
}

impl DeviceMatcher {
    /// Compile device-name globs. `*` spans any run and `?` spans one character.
    ///
    /// # Errors
    ///
    /// The v1 grammar is infallible. The result shape reserves future syntax
    /// validation without changing callers that load configuration.
    pub fn compile<S: AsRef<str>>(globs: &[S]) -> std::result::Result<Self, Infallible> {
        Ok(Self {
            patterns: globs
                .iter()
                .map(|glob| glob.as_ref().to_lowercase().chars().collect())
                .collect(),
        })
    }

    /// Whether a device name matches any configured ignore glob.
    #[must_use]
    pub fn is_ignored(&self, name: &str) -> bool {
        let name: Vec<char> = name.to_lowercase().chars().collect();
        self.patterns
            .iter()
            .any(|pattern| glob_matches(pattern, &name))
    }
}

fn glob_matches(pattern: &[char], text: &[char]) -> bool {
    let (mut pattern_index, mut text_index) = (0, 0);
    let (mut star_index, mut star_text_index) = (None, 0);

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_text_index = text_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }

    pattern[pattern_index..].iter().all(|ch| *ch == '*')
}

/// The sole producer currently authorized to publish input activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAuthority {
    /// The configured compositor, D-Bus, or macOS idle source.
    Stock,
    /// A filtered producer is being prepared while no filtered edge is published.
    StartingFiltered,
    /// The filtered source is the only publishing producer.
    Filtered,
    /// The filtered producer is being cancelled and joined before stock starts.
    StoppingFiltered,
}

/// A filtered input backend controlled by [`InputAuthoritySupervisor`].
#[async_trait::async_trait]
pub trait FilteredInputSource: Send + Sync + 'static {
    /// Open accepted devices and start the publishing reader.
    async fn start(
        &self,
        activity_tx: FilteredActivityTx,
        cancel: CancellationToken,
    ) -> Result<JoinHandle<Result<()>>>;

    /// Prove an accepted device can produce a live read without publishing it.
    async fn probe(&self, cancel: CancellationToken) -> Result<()>;
}

/// Factory for restartable stock idle-source producers.
pub type StockIdleFactory = Arc<dyn Fn() -> Option<Box<dyn IdleSource>> + Send + Sync>;

/// Owns every stock↔filtered transition and never grants two producer tokens.
pub struct InputAuthoritySupervisor {
    filtered: Arc<dyn FilteredInputSource>,
    stock_factory: StockIdleFactory,
    rules: Vec<ActivityRule>,
    poll_interval: Duration,
    idle_tx: Option<IdleObservationTx>,
    filtered_tx: FilteredActivityTx,
    ctl: mpsc::Sender<ControlMsg>,
    authority_tx: watch::Sender<InputAuthority>,
    #[cfg(test)]
    trace: Option<Arc<std::sync::Mutex<Vec<&'static str>>>>,
}

impl InputAuthoritySupervisor {
    /// Build a supervisor. Callers may subscribe before spawning it.
    #[must_use]
    pub fn new(
        filtered: Arc<dyn FilteredInputSource>,
        stock_factory: StockIdleFactory,
        rules: Vec<ActivityRule>,
        poll_interval: Duration,
        idle_tx: Option<IdleObservationTx>,
        filtered_tx: FilteredActivityTx,
        ctl: mpsc::Sender<ControlMsg>,
    ) -> Self {
        let (authority_tx, _) = watch::channel(InputAuthority::StartingFiltered);
        Self {
            filtered,
            stock_factory,
            rules,
            poll_interval,
            idle_tx,
            filtered_tx,
            ctl,
            authority_tx,
            #[cfg(test)]
            trace: None,
        }
    }

    /// Subscribe to explicit authority transitions.
    #[must_use]
    pub fn subscribe_authority(&self) -> watch::Receiver<InputAuthority> {
        self.authority_tx.subscribe()
    }

    #[cfg(test)]
    fn with_test_trace(mut self, trace: Arc<std::sync::Mutex<Vec<&'static str>>>) -> Self {
        self.trace = Some(trace);
        self
    }

    #[allow(
        clippy::unused_self,
        reason = "the receiver carries the cfg(test) trace sink; production intentionally compiles to a no-op"
    )]
    fn record(&self, event: &'static str) {
        #[cfg(test)]
        if let Some(trace) = &self.trace {
            trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
        let _ = event;
    }

    fn set_authority(&self, authority: InputAuthority) {
        self.authority_tx.send_replace(authority);
    }

    /// Run until daemon cancellation, serializing every producer transition.
    pub async fn run(self, cancel: CancellationToken) {
        let mut next_filtered = true;
        let mut global_edge_seq = self.filtered_tx.borrow().edge_seq;

        loop {
            if cancel.is_cancelled() {
                return;
            }

            if next_filtered {
                self.set_authority(InputAuthority::StartingFiltered);
                match self.run_filtered_once(&cancel, &mut global_edge_seq).await {
                    FilteredExit::Cancelled => return,
                    FilteredExit::Unavailable => {
                        next_filtered = false;
                        continue;
                    }
                }
            }

            match self.run_stock_until_recovery(&cancel).await {
                StockExit::Cancelled => return,
                StockExit::RecoveryReady => next_filtered = true,
            }
        }
    }

    async fn run_filtered_once(
        &self,
        cancel: &CancellationToken,
        global_edge_seq: &mut u64,
    ) -> FilteredExit {
        let source_cancel = cancel.child_token();
        let (source_tx, mut source_rx) = filtered_activity_channel();
        let mut handle = match self.filtered.start(source_tx, source_cancel.clone()).await {
            Ok(handle) => handle,
            Err(error) => {
                self.publish_unavailable(*global_edge_seq);
                tracing::warn!(event = "input_filter_unavailable", error = %error);
                return FilteredExit::Unavailable;
            }
        };
        if cancel.is_cancelled() {
            source_cancel.cancel();
            let _ = handle.await;
            return FilteredExit::Cancelled;
        }

        self.record("publishing_filtered_started");
        self.set_authority(InputAuthority::Filtered);
        tracing::info!(event = "input_filter_active");

        let mut last_sent = HashMap::<RuleId, bool>::new();
        let mut source_edge_seq = 0;
        let stale_after = self.poll_interval.saturating_mul(2);
        let stale_sleep = tokio::time::sleep_until(tokio::time::Instant::now() + stale_after);
        tokio::pin!(stale_sleep);
        let mut tick = tokio::time::interval(self.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let unavailable_reason = loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    source_cancel.cancel();
                    let _ = handle.await;
                    return FilteredExit::Cancelled;
                }
                result = &mut handle => {
                    break match result {
                        Ok(Ok(())) => "filtered activity watch closed",
                        Ok(Err(ref error)) => {
                            tracing::debug!(event = "input_filter_reader_failed", error = %error);
                            "filtered activity reader failed"
                        }
                        Err(ref error) => {
                            tracing::debug!(event = "input_filter_reader_join_failed", error = %error);
                            "filtered activity reader join failed"
                        }
                    };
                }
                changed = source_rx.changed() => {
                    if changed.is_err() {
                        break "filtered activity watch closed";
                    }
                    let observation = source_rx.borrow_and_update().clone();
                    if !observation.available {
                        break "filtered activity became unavailable";
                    }
                    let observation = remap_edge_sequence(
                        observation,
                        &mut source_edge_seq,
                        global_edge_seq,
                    );
                    self.publish_filtered(&observation, &mut last_sent);
                    stale_sleep.as_mut().reset(tokio::time::Instant::now() + stale_after);
                }
                _ = tick.tick() => {
                    let observation = source_rx.borrow().clone();
                    if observation.available {
                        let observation = remap_edge_sequence(
                            observation,
                            &mut source_edge_seq,
                            global_edge_seq,
                        );
                        self.publish_filtered(&observation, &mut last_sent);
                    }
                }
                () = &mut stale_sleep => {
                    break "filtered activity watch stale";
                }
            }
        };

        self.set_authority(InputAuthority::StoppingFiltered);
        self.publish_unavailable(*global_edge_seq);
        source_cancel.cancel();
        if !handle.is_finished() {
            let _ = handle.await;
        }
        tracing::warn!(
            event = "input_filter_unavailable",
            reason = unavailable_reason,
        );
        FilteredExit::Unavailable
    }

    async fn run_stock_until_recovery(&self, cancel: &CancellationToken) -> StockExit {
        loop {
            self.set_authority(InputAuthority::Stock);
            let stock_cancel = cancel.child_token();
            let Some(source) = (self.stock_factory)() else {
                self.hold_awake();
                if idle_source::sleep_or_cancel(self.poll_interval, cancel).await {
                    return StockExit::Cancelled;
                }
                continue;
            };
            let ctl = self.ctl.clone();
            let run_cancel = stock_cancel.clone();
            let mut stock_handle = tokio::spawn(async move { source.run(ctl, run_cancel).await });
            let probe_cancel = cancel.child_token();
            let probe = self.filtered.probe(probe_cancel.clone());
            tokio::pin!(probe);

            let probe_result = tokio::select! {
                () = cancel.cancelled() => {
                    probe_cancel.cancel();
                    stock_cancel.cancel();
                    let _ = stock_handle.await;
                    return StockExit::Cancelled;
                }
                result = &mut stock_handle => {
                    tracing::warn!(event = "stock_idle_source_exited", result = ?result);
                    probe_cancel.cancel();
                    continue;
                }
                result = &mut probe => result,
            };

            probe_cancel.cancel();
            match probe_result {
                Ok(()) => {
                    self.record("non_publishing_probe_ready");
                    self.set_authority(InputAuthority::StartingFiltered);
                    self.record("stock_cancelled");
                    stock_cancel.cancel();
                    let _ = stock_handle.await;
                    self.record("stock_joined");
                    return StockExit::RecoveryReady;
                }
                Err(error) => {
                    tracing::debug!(event = "input_filter_probe_failed", error = %error);
                    if idle_source::sleep_or_cancel(self.poll_interval, cancel).await {
                        stock_cancel.cancel();
                        let _ = stock_handle.await;
                        return StockExit::Cancelled;
                    }
                    stock_cancel.cancel();
                    let _ = stock_handle.await;
                }
            }
        }
    }

    fn publish_filtered(
        &self,
        observation: &FilteredActivity,
        last_sent: &mut HashMap<RuleId, bool>,
    ) {
        self.filtered_tx.send_replace(observation.clone());
        if let Some(idle_tx) = &self.idle_tx {
            idle_tx.send_replace(IdleObservation {
                last_activity: observation.last_activity,
                observed_at: observation.observed_at,
                available: observation.available,
            });
        }
        let now = Instant::now();
        let idle = observation
            .last_activity
            .filter(|last| *last <= now)
            .map(|last| now.saturating_duration_since(last));
        for rule in &self.rules {
            idle_source::publish(
                &self.ctl,
                last_sent,
                &rule.rule,
                idle.is_none_or(|idle| idle < rule.idle_threshold),
            );
        }
    }

    fn publish_unavailable(&self, edge_seq: u64) {
        let unavailable = FilteredActivity {
            edge_seq,
            ..FilteredActivity::unavailable()
        };
        self.filtered_tx.send_replace(unavailable.clone());
        if let Some(idle_tx) = &self.idle_tx {
            idle_tx.send_replace(IdleObservation {
                last_activity: None,
                observed_at: unavailable.observed_at,
                available: false,
            });
        }
        self.hold_awake();
    }

    fn hold_awake(&self) {
        let mut last_sent = HashMap::new();
        for rule in &self.rules {
            idle_source::publish(&self.ctl, &mut last_sent, &rule.rule, true);
        }
    }
}

fn remap_edge_sequence(
    mut observation: FilteredActivity,
    source_edge_seq: &mut u64,
    global_edge_seq: &mut u64,
) -> FilteredActivity {
    let new_edges = observation.edge_seq.saturating_sub(*source_edge_seq);
    *global_edge_seq = global_edge_seq.saturating_add(new_edges);
    *source_edge_seq = observation.edge_seq;
    observation.edge_seq = *global_edge_seq;
    observation
}

enum FilteredExit {
    Cancelled,
    Unavailable,
}

enum StockExit {
    Cancelled,
    RecoveryReady,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow, bail};
    use dormant_core::rules::{ControlMsg, InhibitorKind};
    use dormant_core::types::RuleId;
    use test_case::test_case;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::{
        DeviceMatcher, FilteredActivity, FilteredActivityTx, FilteredInputSource, InputAuthority,
        InputAuthoritySupervisor, StockIdleFactory, filtered_activity_channel,
    };
    use crate::idle_observation::{IdleObservation, IdleObservationRx, idle_observation_channel};
    use crate::idle_source::{ActivityRule, IdleSource};

    #[test_case("USB JIGGLER", &["*jiggler*"], true)]
    #[test_case("MosArt Keyboard", &["MosArt *"], true)]
    #[test_case("Gaming Mouse", &["*jiggler*"], false)]
    fn device_name_globs_are_case_insensitive(name: &str, globs: &[&str], ignored: bool) {
        assert_eq!(
            DeviceMatcher::compile(globs).unwrap().is_ignored(name),
            ignored
        );
    }

    #[tokio::test]
    async fn one_reader_fans_same_edge_to_all_consumers() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.emit_key().await;
        assert_eq!(harness.inhibitor_edge_seq(), 1);
        assert_eq!(harness.claim_edge_seq(), 1);
        assert_eq!(harness.wake_edge_seq(), 1);
        assert_eq!(harness.device_open_count(), 1);
    }

    #[tokio::test]
    async fn ignored_device_activity_neither_holds_awake_nor_creates_a_claim_edge() {
        let harness = FilteredActivityHarness::new([
            FakeDevice::accepted("keyboard"),
            FakeDevice::ignored("USB Jiggler"),
        ])
        .await;
        harness.emit_device_key("USB Jiggler");
        tokio::task::yield_now().await;
        assert_eq!(harness.inhibitor_edge_seq(), 0);
        assert_eq!(harness.claim_edge_seq(), 0);
    }

    #[tokio::test]
    async fn accepted_device_hotplug_recovers_filtered_authority() {
        let harness = FilteredActivityHarness::new([FakeDevice::ignored("USB Jiggler")]).await;
        harness.wait_for_authority(InputAuthority::Stock).await;
        harness.hotplug(FakeDevice::accepted("keyboard"));
        harness.prove_recovery_with_key().await;
        harness.wait_for_authority(InputAuthority::Filtered).await;
    }

    #[tokio::test]
    async fn last_accepted_device_unplug_falls_back_to_stock() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.wait_for_authority(InputAuthority::Filtered).await;
        harness.clear_control_messages().await;
        harness.unplug("keyboard");
        harness.wait_for_authority(InputAuthority::Stock).await;
        assert!(!harness.filtered_observation().available);
        assert!(harness.stock_observation().available);
        harness.assert_hold_awake_emitted().await;
    }

    #[tokio::test]
    async fn permission_revocation_is_unavailable_then_falls_back_to_stock() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.clear_control_messages().await;
        harness.revoke_permissions();
        harness.wait_for_authority(InputAuthority::Stock).await;
        assert!(!harness.filtered_observation().available);
        assert!(harness.stock_observation().available);
        harness.assert_hold_awake_emitted().await;
    }

    #[tokio::test]
    async fn ignored_only_fleet_falls_back_to_stock_fail_safe() {
        let harness = FilteredActivityHarness::new([FakeDevice::ignored("USB Jiggler")]).await;
        harness.wait_for_authority(InputAuthority::Stock).await;
        assert!(!harness.filtered_observation().available);
        assert!(harness.stock_observation().available);
        harness.assert_hold_awake_emitted().await;
    }

    #[tokio::test]
    async fn activity_uses_monotonic_observation_and_activity_timestamps() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        let before = Instant::now();
        harness.emit_key().await;
        let observation = harness.filtered_observation();
        assert!(observation.observed_at >= before);
        assert!(observation.last_activity.is_some_and(|at| at >= before));
    }

    #[tokio::test]
    async fn watch_coalescing_preserves_the_latest_edge_sequence() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.emit_key_without_wait();
        harness.emit_key_without_wait();
        harness.wait_for_edge(2).await;
        assert_eq!(harness.filtered_observation().edge_seq, 2);
    }

    #[tokio::test]
    async fn closed_filtered_watch_falls_back_to_stock() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.clear_control_messages().await;
        harness.close_reader();
        harness.wait_for_authority(InputAuthority::Stock).await;
        assert!(!harness.filtered_observation().available);
        assert!(harness.stock_observation().available);
        harness.assert_hold_awake_emitted().await;
    }

    #[tokio::test(start_paused = true)]
    async fn stale_filtered_watch_falls_back_to_stock() {
        let harness = FilteredActivityHarness::new_with_poll(
            [FakeDevice::accepted("keyboard")],
            Duration::from_secs(1),
        )
        .await;
        harness.clear_control_messages().await;
        harness.stop_heartbeats();
        tokio::time::advance(Duration::from_secs(3)).await;
        harness.wait_for_authority(InputAuthority::Stock).await;
        assert!(!harness.filtered_observation().available);
        assert!(harness.stock_observation().available);
        harness.assert_hold_awake_emitted().await;
    }

    #[tokio::test]
    async fn cancellation_stops_the_only_live_authority() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.cancel().await;
        assert_eq!(harness.live_producer_count(), 0);
    }

    #[tokio::test]
    async fn recovery_probation_never_publishes_an_activity_edge() {
        let harness = FilteredActivityHarness::new([FakeDevice::ignored("USB Jiggler")]).await;
        harness.pause_recovery_start();
        harness.hotplug(FakeDevice::accepted("keyboard"));
        harness.prove_recovery_with_key().await;
        harness.wait_for_recovery_start().await;
        assert_eq!(
            harness.recovery_trace(),
            [
                "non_publishing_probe_ready",
                "stock_cancelled",
                "stock_joined",
            ]
        );
        assert_eq!(
            harness.current_authority().await,
            InputAuthority::StartingFiltered
        );
        assert_eq!(harness.filtered_observation().edge_seq, 0);
        harness.release_recovery_start();
        harness.wait_for_authority(InputAuthority::Filtered).await;
    }

    #[tokio::test]
    async fn cancellation_during_filtered_start_never_records_filtered_authority() {
        let harness = FilteredActivityHarness::new([FakeDevice::ignored("USB Jiggler")]).await;
        harness.pause_recovery_start();
        harness.hotplug(FakeDevice::accepted("keyboard"));
        harness.prove_recovery_with_key().await;
        harness.wait_for_recovery_start().await;
        harness.cancel_during_pending_start().await;
        assert_ne!(harness.current_authority().await, InputAuthority::Filtered);
    }

    #[tokio::test]
    async fn recovery_order_is_probe_then_stock_join_then_filtered_publish() {
        let harness = FilteredActivityHarness::new([FakeDevice::ignored("USB Jiggler")]).await;
        harness.hotplug(FakeDevice::accepted("keyboard"));
        harness.prove_recovery_with_key().await;
        harness.wait_for_authority(InputAuthority::Filtered).await;
        assert_eq!(
            harness.recovery_trace(),
            [
                "non_publishing_probe_ready",
                "stock_cancelled",
                "stock_joined",
                "publishing_filtered_started",
            ]
        );
    }

    #[tokio::test]
    async fn stock_and_filtered_producers_are_never_live_together() {
        let harness = FilteredActivityHarness::new([FakeDevice::ignored("USB Jiggler")]).await;
        harness.hotplug(FakeDevice::accepted("keyboard"));
        harness.prove_recovery_with_key().await;
        harness.wait_for_authority(InputAuthority::Filtered).await;
        harness.unplug("keyboard");
        harness.wait_for_authority(InputAuthority::Stock).await;
        assert_eq!(harness.maximum_live_producer_count(), 1);
    }

    #[tokio::test]
    async fn edge_sequence_remains_monotonic_across_filtered_recovery() {
        let harness = FilteredActivityHarness::new([FakeDevice::accepted("keyboard")]).await;
        harness.emit_key().await;
        harness.unplug("keyboard");
        harness.wait_for_authority(InputAuthority::Stock).await;
        harness.hotplug(FakeDevice::accepted("keyboard"));
        harness.prove_recovery_with_key().await;
        harness.wait_for_authority(InputAuthority::Filtered).await;
        harness.emit_key().await;
        assert_eq!(harness.filtered_observation().edge_seq, 2);
    }

    #[test]
    fn stale_or_unavailable_activity_cannot_validate_a_consumer_edge() {
        let now = Instant::now();
        let stale_at = now.checked_sub(Duration::from_secs(2)).unwrap();
        let stale = FilteredActivity {
            last_activity: Some(stale_at),
            observed_at: stale_at,
            available: true,
            edge_seq: 1,
        };
        let unavailable = FilteredActivity {
            available: false,
            ..stale.clone()
        };
        assert!(!stale.is_recent(now, Duration::from_millis(500)));
        assert!(!unavailable.is_recent(now, Duration::from_secs(5)));
    }

    #[derive(Clone)]
    struct FakeDevice {
        name: String,
        ignored: bool,
    }

    impl FakeDevice {
        fn accepted(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                ignored: false,
            }
        }

        fn ignored(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                ignored: true,
            }
        }
    }

    #[derive(Clone)]
    enum FakeEvent {
        Activity(String),
        InventoryChanged,
        PermissionRevoked,
        Close,
    }

    struct FakeSourceState {
        devices: Mutex<HashMap<String, FakeDevice>>,
        revoked: AtomicBool,
        heartbeat: AtomicBool,
        event_tx: broadcast::Sender<FakeEvent>,
        probe_listening: AtomicBool,
        probe_notify: tokio::sync::Notify,
        pause_start: AtomicBool,
        start_waiting: AtomicBool,
        start_notify: tokio::sync::Notify,
        start_release: tokio::sync::Notify,
        open_count: AtomicUsize,
        live: Arc<AtomicUsize>,
        maximum_live: Arc<AtomicUsize>,
        poll_interval: Duration,
    }

    impl FakeSourceState {
        fn accepted_count(&self) -> usize {
            self.devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter(|device| !device.ignored)
                .count()
        }

        fn can_open(&self) -> bool {
            !self.revoked.load(Ordering::SeqCst) && self.accepted_count() > 0
        }
    }

    struct FakeFilteredSource {
        state: Arc<FakeSourceState>,
    }

    #[async_trait::async_trait]
    impl FilteredInputSource for FakeFilteredSource {
        async fn start(
            &self,
            activity_tx: FilteredActivityTx,
            cancel: CancellationToken,
        ) -> Result<JoinHandle<Result<()>>> {
            if !self.state.can_open() {
                bail!("no accepted fake device is readable");
            }
            if self.state.pause_start.load(Ordering::SeqCst) {
                self.state.start_waiting.store(true, Ordering::SeqCst);
                self.state.start_notify.notify_waiters();
                self.state.start_release.notified().await;
                self.state.start_waiting.store(false, Ordering::SeqCst);
            }
            self.state
                .open_count
                .fetch_add(self.state.accepted_count(), Ordering::SeqCst);
            let now = Instant::now();
            activity_tx.send_replace(FilteredActivity {
                last_activity: Some(now),
                observed_at: now,
                available: true,
                edge_seq: 0,
            });
            let state = self.state.clone();
            let mut event_rx = state.event_tx.subscribe();
            let guard = ProducerGuard::new(state.live.clone(), &state.maximum_live);
            Ok(tokio::spawn(async move {
                let _guard = guard;
                let mut observation = activity_tx.borrow().clone();
                let mut heartbeat = tokio::time::interval(state.poll_interval);
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(()),
                        event = event_rx.recv() => match event {
                            Ok(FakeEvent::Activity(name)) => {
                                let accepted = state
                                    .devices
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .get(&name)
                                    .is_some_and(|device| !device.ignored);
                                if !accepted {
                                    continue;
                                }
                                let now = Instant::now();
                                observation.last_activity = Some(now);
                                observation.observed_at = now;
                                observation.edge_seq = observation.edge_seq.saturating_add(1);
                                activity_tx.send_replace(observation.clone());
                            }
                            Ok(FakeEvent::InventoryChanged) => {
                                if !state.can_open() {
                                    observation.available = false;
                                    observation.observed_at = Instant::now();
                                    activity_tx.send_replace(observation);
                                    bail!("last accepted fake device was unplugged");
                                }
                            }
                            Ok(FakeEvent::PermissionRevoked) => {
                                observation.available = false;
                                observation.observed_at = Instant::now();
                                activity_tx.send_replace(observation);
                                bail!("fake device permission revoked");
                            }
                            Ok(FakeEvent::Close) | Err(broadcast::error::RecvError::Closed) => {
                                return Ok(());
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                        },
                        _ = heartbeat.tick(), if state.heartbeat.load(Ordering::SeqCst) => {
                            observation.observed_at = Instant::now();
                            activity_tx.send_replace(observation.clone());
                        }
                    }
                }
            }))
        }

        async fn probe(&self, cancel: CancellationToken) -> Result<()> {
            let mut event_rx = self.state.event_tx.subscribe();
            if self.state.can_open() {
                self.state.probe_listening.store(true, Ordering::SeqCst);
                self.state.probe_notify.notify_waiters();
            }
            let result = loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        break Err(anyhow!("fake recovery probe cancelled"));
                    }
                    event = event_rx.recv() => match event {
                        Ok(FakeEvent::Activity(name)) => {
                            let accepted = self.state
                                .devices
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(&name)
                                .is_some_and(|device| !device.ignored);
                            if accepted {
                                break Ok(());
                            }
                        }
                        Ok(FakeEvent::InventoryChanged) => {
                            if self.state.can_open() {
                                self.state.probe_listening.store(true, Ordering::SeqCst);
                                self.state.probe_notify.notify_waiters();
                            } else {
                                self.state.probe_listening.store(false, Ordering::SeqCst);
                            }
                        }
                        Ok(FakeEvent::PermissionRevoked) => {
                            break Err(anyhow!("fake recovery probe permission revoked"));
                        }
                        Ok(FakeEvent::Close) | Err(broadcast::error::RecvError::Closed) => {
                            break Err(anyhow!("fake recovery probe closed"));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
            };
            self.state.probe_listening.store(false, Ordering::SeqCst);
            result
        }
    }

    struct FakeStockSource {
        idle_tx: crate::idle_observation::IdleObservationTx,
        live: Arc<AtomicUsize>,
        maximum_live: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl IdleSource for FakeStockSource {
        async fn run(self: Box<Self>, _ctl: mpsc::Sender<ControlMsg>, cancel: CancellationToken) {
            let _guard = ProducerGuard::new(self.live.clone(), &self.maximum_live);
            let now = Instant::now();
            self.idle_tx.send_replace(IdleObservation {
                last_activity: Some(now),
                observed_at: now,
                available: true,
            });
            cancel.cancelled().await;
        }
    }

    struct ProducerGuard {
        live: Arc<AtomicUsize>,
    }

    impl ProducerGuard {
        fn new(live: Arc<AtomicUsize>, maximum_live: &AtomicUsize) -> Self {
            let current = live.fetch_add(1, Ordering::SeqCst) + 1;
            maximum_live.fetch_max(current, Ordering::SeqCst);
            Self { live }
        }
    }

    impl Drop for ProducerGuard {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct FilteredActivityHarness {
        state: Arc<FakeSourceState>,
        filtered_rx: watch::Receiver<FilteredActivity>,
        inhibitor_rx: watch::Receiver<FilteredActivity>,
        claim_rx: watch::Receiver<FilteredActivity>,
        wake_rx: watch::Receiver<FilteredActivity>,
        idle_rx: IdleObservationRx,
        authority_rx: tokio::sync::Mutex<watch::Receiver<InputAuthority>>,
        cancel: CancellationToken,
        handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
        trace: Arc<Mutex<Vec<&'static str>>>,
        ctl_rx: tokio::sync::Mutex<mpsc::Receiver<ControlMsg>>,
    }

    impl FilteredActivityHarness {
        async fn new(devices: impl IntoIterator<Item = FakeDevice>) -> Self {
            Self::new_with_poll(devices, Duration::from_secs(60)).await
        }

        async fn new_with_poll(
            devices: impl IntoIterator<Item = FakeDevice>,
            poll_interval: Duration,
        ) -> Self {
            let devices: HashMap<String, FakeDevice> = devices
                .into_iter()
                .map(|device| (device.name.clone(), device))
                .collect();
            let initial_filtered = devices.values().any(|device| !device.ignored);
            let (event_tx, _) = broadcast::channel(32);
            let live = Arc::new(AtomicUsize::new(0));
            let maximum_live = Arc::new(AtomicUsize::new(0));
            let state = Arc::new(FakeSourceState {
                devices: Mutex::new(devices),
                revoked: AtomicBool::new(false),
                heartbeat: AtomicBool::new(true),
                event_tx,
                probe_listening: AtomicBool::new(false),
                probe_notify: tokio::sync::Notify::new(),
                pause_start: AtomicBool::new(false),
                start_waiting: AtomicBool::new(false),
                start_notify: tokio::sync::Notify::new(),
                start_release: tokio::sync::Notify::new(),
                open_count: AtomicUsize::new(0),
                live: live.clone(),
                maximum_live: maximum_live.clone(),
                poll_interval,
            });
            let filtered = Arc::new(FakeFilteredSource {
                state: state.clone(),
            });
            let (filtered_tx, filtered_rx) = filtered_activity_channel();
            let inhibitor_rx = filtered_rx.clone();
            let claim_rx = filtered_rx.clone();
            let wake_rx = filtered_rx.clone();
            let (idle_tx, idle_rx) = idle_observation_channel();
            let stock_idle_tx = idle_tx.clone();
            let stock_live = live;
            let stock_maximum_live = maximum_live;
            let stock_factory: StockIdleFactory = Arc::new(move || {
                Some(Box::new(FakeStockSource {
                    idle_tx: stock_idle_tx.clone(),
                    live: stock_live.clone(),
                    maximum_live: stock_maximum_live.clone(),
                }))
            });
            let (ctl_tx, ctl_rx) = mpsc::channel(64);
            let trace = Arc::new(Mutex::new(Vec::new()));
            let supervisor = InputAuthoritySupervisor::new(
                filtered,
                stock_factory,
                vec![ActivityRule {
                    rule: RuleId("activity".to_owned()),
                    idle_threshold: Duration::from_secs(30),
                }],
                poll_interval,
                Some(idle_tx),
                filtered_tx,
                ctl_tx,
            )
            .with_test_trace(trace.clone());
            let authority_rx = supervisor.subscribe_authority();
            let cancel = CancellationToken::new();
            let handle = tokio::spawn(supervisor.run(cancel.clone()));
            let harness = Self {
                state,
                filtered_rx,
                inhibitor_rx,
                claim_rx,
                wake_rx,
                idle_rx,
                authority_rx: tokio::sync::Mutex::new(authority_rx),
                cancel,
                handle: tokio::sync::Mutex::new(Some(handle)),
                trace,
                ctl_rx: tokio::sync::Mutex::new(ctl_rx),
            };
            harness
                .wait_for_authority(if initial_filtered {
                    InputAuthority::Filtered
                } else {
                    InputAuthority::Stock
                })
                .await;
            harness
        }

        async fn wait_for_authority(&self, wanted: InputAuthority) {
            let mut rx = self.authority_rx.lock().await;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if *rx.borrow() == wanted {
                        return;
                    }
                    rx.changed().await.expect("authority sender remains live");
                }
            })
            .await
            .expect("authority transition timed out");
        }

        async fn wait_for_edge(&self, wanted: u64) {
            let mut rx = self.filtered_rx.clone();
            tokio::time::timeout(Duration::from_secs(2), async move {
                loop {
                    if rx.borrow().edge_seq >= wanted {
                        return;
                    }
                    rx.changed().await.expect("filtered sender remains live");
                }
            })
            .await
            .expect("filtered edge timed out");
        }

        async fn emit_key(&self) {
            let next = self.filtered_observation().edge_seq.saturating_add(1);
            let name = self
                .state
                .devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .find(|device| !device.ignored)
                .expect("accepted fake device")
                .name
                .clone();
            self.emit_device_key(&name);
            self.wait_for_edge(next).await;
        }

        fn emit_key_without_wait(&self) {
            let name = self
                .state
                .devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .find(|device| !device.ignored)
                .expect("accepted fake device")
                .name
                .clone();
            self.emit_device_key(&name);
        }

        fn emit_device_key(&self, name: &str) {
            let _ = self
                .state
                .event_tx
                .send(FakeEvent::Activity(name.to_owned()));
        }

        fn hotplug(&self, device: FakeDevice) {
            self.state
                .devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(device.name.clone(), device);
            let _ = self.state.event_tx.send(FakeEvent::InventoryChanged);
        }

        fn unplug(&self, name: &str) {
            self.state
                .devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(name);
            let _ = self.state.event_tx.send(FakeEvent::InventoryChanged);
        }

        fn revoke_permissions(&self) {
            self.state.revoked.store(true, Ordering::SeqCst);
            let _ = self.state.event_tx.send(FakeEvent::PermissionRevoked);
        }

        fn close_reader(&self) {
            let _ = self.state.event_tx.send(FakeEvent::Close);
        }

        fn stop_heartbeats(&self) {
            self.state.heartbeat.store(false, Ordering::SeqCst);
        }

        async fn prove_recovery_with_key(&self) {
            loop {
                let notified = self.state.probe_notify.notified();
                if self.state.probe_listening.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
            self.emit_key_without_wait();
        }

        fn pause_recovery_start(&self) {
            self.state.pause_start.store(true, Ordering::SeqCst);
        }

        async fn wait_for_recovery_start(&self) {
            loop {
                let notified = self.state.start_notify.notified();
                if self.state.start_waiting.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        }

        fn release_recovery_start(&self) {
            self.state.start_release.notify_waiters();
        }

        async fn current_authority(&self) -> InputAuthority {
            *self.authority_rx.lock().await.borrow()
        }

        async fn clear_control_messages(&self) {
            let _ = self.drain_control_messages().await;
        }

        async fn assert_hold_awake_emitted(&self) {
            let messages = self.drain_control_messages().await;
            assert!(messages.iter().any(|message| matches!(
                message,
                ControlMsg::SetInhibited {
                    kind: InhibitorKind::UserActivity,
                    inhibited: true,
                    ..
                }
            )));
        }

        async fn drain_control_messages(&self) -> Vec<ControlMsg> {
            let mut rx = self.ctl_rx.lock().await;
            let mut messages = Vec::new();
            while let Ok(message) = rx.try_recv() {
                messages.push(message);
            }
            messages
        }

        fn filtered_observation(&self) -> FilteredActivity {
            self.filtered_rx.borrow().clone()
        }

        fn stock_observation(&self) -> IdleObservation {
            self.idle_rx.borrow().clone()
        }

        fn inhibitor_edge_seq(&self) -> u64 {
            self.inhibitor_rx.borrow().edge_seq
        }

        fn claim_edge_seq(&self) -> u64 {
            self.claim_rx.borrow().edge_seq
        }

        fn wake_edge_seq(&self) -> u64 {
            self.wake_rx.borrow().edge_seq
        }

        fn device_open_count(&self) -> usize {
            self.state.open_count.load(Ordering::SeqCst)
        }

        fn live_producer_count(&self) -> usize {
            self.state.live.load(Ordering::SeqCst)
        }

        fn maximum_live_producer_count(&self) -> usize {
            self.state.maximum_live.load(Ordering::SeqCst)
        }

        fn recovery_trace(&self) -> Vec<&'static str> {
            self.trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn cancel(&self) {
            self.cancel.cancel();
            if let Some(handle) = self.handle.lock().await.take() {
                handle.await.expect("supervisor joins");
            }
        }

        async fn cancel_during_pending_start(&self) {
            self.cancel.cancel();
            self.release_recovery_start();
            if let Some(handle) = self.handle.lock().await.take() {
                handle.await.expect("supervisor joins");
            }
        }
    }

    impl Drop for FilteredActivityHarness {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
}
