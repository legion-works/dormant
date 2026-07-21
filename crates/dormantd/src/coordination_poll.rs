//! Periodic shared-display ownership polling.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dormant_core::config::{Config, DisplayScope};
use dormant_core::coordination::{COORD_POLL_FAILING_LOG_INTERVAL, CoordinationHandle};
use dormant_core::rules::ControlMsg;
use dormant_core::traits::CommandSink;
use dormant_core::types::DisplayId;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Dependencies required by the shared-display ownership poller.
pub struct CoordinationPollDeps {
    /// Reloadable configuration, including the polling cadence and shared displays.
    pub config_rx: watch::Receiver<Arc<Config>>,
    /// Front control channel, which remains valid through generation swaps.
    pub ctl_tx: mpsc::Sender<ControlMsg>,
    /// Current generation's display executors.
    pub executors_rx: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    /// Daemon-lifetime ownership verdict cache.
    pub state: CoordinationHandle,
    /// Daemon-lifetime cancellation token.
    pub cancel: CancellationToken,
}

/// Spawn the shared-display ownership poller.
#[must_use]
pub fn spawn(deps: CoordinationPollDeps) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(deps))
}

async fn run(mut deps: CoordinationPollDeps) {
    let mut interval = new_interval(deps.config_rx.borrow().coordination.poll_interval);
    let mut last_failing_log = HashMap::new();
    loop {
        tokio::select! {
            () = deps.cancel.cancelled() => break,
            changed = deps.config_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                interval = new_interval(deps.config_rx.borrow().coordination.poll_interval);
            }
            _ = interval.tick() => poll_once(&deps, &mut last_failing_log).await,
        }
    }
}

fn new_interval(period: Duration) -> tokio::time::Interval {
    let period = period.max(Duration::from_millis(1));
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

async fn poll_once(
    deps: &CoordinationPollDeps,
    last_failing_log: &mut HashMap<DisplayId, Instant>,
) {
    let executors = deps.executors_rx.borrow().clone();
    // Reload intentionally publishes this sentinel while an old generation tears down.
    if executors.is_empty() {
        return;
    }

    let config = deps.config_rx.borrow().clone();
    for (name, display_config) in &config.displays {
        if display_config.scope != DisplayScope::Shared {
            continue;
        }
        let Some(expected) = display_config.shared_input_code else {
            continue;
        };
        let display_id = DisplayId(name.clone());
        let Some(executor) = executors.get(&display_id) else {
            continue;
        };

        // Hardware reads may wait on controller locks; mutate only after both finish.
        let input = executor.read_input_source_sampled().await;
        let panel_state = executor.read_state_sampled().await;
        if let Ok(Some(observed)) = input {
            let before = deps.state.snapshot();
            let prior = deps
                .state
                .record_success(&display_id, observed, expected, panel_state);
            let previous = before.get(&display_id);
            if previous.is_some_and(|record| {
                !record.has_successful_input_read || record.consecutive_failures > 0
            }) {
                tracing::info!(event = "coord_poll_ok", display = %display_id);
            }
            last_failing_log.remove(&display_id);
            if let Some(previous_owned) = prior {
                let owned = observed == expected;
                tracing::info!(event = "coord_ownership_changed", display = %display_id, previous_owned, owned);
                let _ = deps
                    .ctl_tx
                    .send(ControlMsg::OwnershipPoll {
                        display: display_id,
                    })
                    .await;
            }
        } else {
            deps.state.record_failure(&display_id);
            let failures = deps
                .state
                .snapshot()
                .get(&display_id)
                .map_or(0, |record| record.consecutive_failures);
            let now = Instant::now();
            if failures >= 2
                && last_failing_log
                    .get(&display_id)
                    .is_none_or(|last| now.duration_since(*last) >= COORD_POLL_FAILING_LOG_INTERVAL)
            {
                tracing::warn!(event = "coord_poll_failing", display = %display_id, consecutive_failures = failures);
                last_failing_log.insert(display_id, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CoordinationPollDeps, spawn};
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, DisplayConfig, DisplayScope, NotificationsConfig,
        WatchdogConfig, WearConfig,
    };
    use dormant_core::config::{Config, CoordinationConfig};
    use dormant_core::coordination::{COORD_POLL_FAILING_LOG_INTERVAL, CoordinationHandle};
    use dormant_core::rules::{ControlMsg, ControllerHealth};
    use dormant_core::traits::{CommandSink, PanelState};
    use dormant_core::types::{BlankMode, CmdFailure, DisplayId};
    use dormant_core::wear::PanelType;
    use indexmap::IndexMap;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;

    type TestHarness = (
        watch::Sender<Arc<Config>>,
        watch::Sender<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
        mpsc::Receiver<ControlMsg>,
        CoordinationHandle,
        CancellationToken,
    );

    #[derive(Default)]
    struct ScriptedSink {
        inputs: Mutex<VecDeque<Result<Option<u8>, String>>>,
        states: Mutex<VecDeque<Option<PanelState>>>,
        reads: Mutex<u32>,
        cache_probe: Mutex<Option<CoordinationHandle>>,
    }

    impl ScriptedSink {
        fn with_inputs(inputs: impl IntoIterator<Item = Result<Option<u8>, String>>) -> Self {
            Self {
                inputs: Mutex::new(inputs.into_iter().collect()),
                ..Self::default()
            }
        }

        fn reads(&self) -> u32 {
            *self.reads.lock().unwrap()
        }

        fn probe_cache(&self, state: CoordinationHandle) {
            *self.cache_probe.lock().unwrap() = Some(state);
        }
    }

    #[async_trait::async_trait]
    impl CommandSink for ScriptedSink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Ok(())
        }

        async fn wake(&self) -> Result<(), CmdFailure> {
            Ok(())
        }

        fn controller_health(&self) -> Vec<ControllerHealth> {
            Vec::new()
        }

        async fn read_input_source_sampled(&self) -> Result<Option<u8>, String> {
            *self.reads.lock().unwrap() += 1;
            let _ = self
                .cache_probe
                .lock()
                .unwrap()
                .as_ref()
                .map(CoordinationHandle::snapshot);
            self.inputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Some(0x11)))
        }

        async fn read_state_sampled(&self) -> Option<PanelState> {
            self.states.lock().unwrap().pop_front().unwrap_or(None)
        }
    }

    fn config() -> Config {
        let mut displays = IndexMap::new();
        displays.insert(
            "shared".to_string(),
            DisplayConfig {
                controllers: vec!["ddcci".to_string()],
                scope: DisplayScope::Shared,
                shared_input_code: Some(0x11),
                blank_mode: None,
                degraded_mode: None,
                ladder: Vec::new(),
                screensaver: None,
                output: None,
                ddc_display: None,
                host: None,
                wol_mac: None,
                blank_command: None,
                wake_command: None,
                modes: None,
                ha_url: None,
                blank_service: None,
                blank_data: None,
                wake_service: None,
                wake_data: None,
                command_timeout: Duration::from_secs(5),
                restore_brightness: 80,
                samsung_restore_backlight: 50,
                treat_unreachable_as_blanked: true,
                panel_type: PanelType::Unknown,
            },
        );
        Config {
            coordination: CoordinationConfig {
                poll_interval: Duration::from_secs(6),
                ..CoordinationConfig::default()
            },
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays,
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
        }
    }

    fn setup(sink: Arc<ScriptedSink>) -> TestHarness {
        let (config_tx, config_rx) = watch::channel(Arc::new(config()));
        let display = DisplayId("shared".to_string());
        let executors = HashMap::from([(display.clone(), sink as Arc<dyn CommandSink>)]);
        let (executors_tx, executors_rx) = watch::channel(Arc::new(executors));
        let (ctl_tx, ctl_rx) = mpsc::channel(8);
        let state = CoordinationHandle::new([display]);
        let cancel = CancellationToken::new();
        let _task = spawn(CoordinationPollDeps {
            config_rx,
            ctl_tx,
            executors_rx,
            state: state.clone(),
            cancel: cancel.clone(),
        });
        (config_tx, executors_tx, ctl_rx, state, cancel)
    }

    async fn tick() {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cold_start_failure_keeps_owned_true_without_poke() {
        let sink = Arc::new(ScriptedSink::with_inputs([Err("no readback".to_string())]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup(sink);
        tick().await;
        let record = state
            .snapshot()
            .remove(&DisplayId("shared".to_string()))
            .unwrap();
        assert!(record.owned);
        assert!(!state.has_successful_read(&DisplayId("shared".to_string())));
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn successful_other_input_changes_false_and_pokes_once() {
        let sink = Arc::new(ScriptedSink::with_inputs([Ok(Some(0x12))]));
        let (_config_tx, _executors_tx, mut ctl_rx, _state, cancel) = setup(sink);
        tick().await;
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { .. })
        ));
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn second_same_verdict_success_does_not_poke_again() {
        let sink = Arc::new(ScriptedSink::with_inputs([Ok(Some(0x12)), Ok(Some(0x12))]));
        let (_config_tx, _executors_tx, mut ctl_rx, _state, cancel) = setup(sink);
        tick().await;
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { .. })
        ));
        tick().await;
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn each_shared_display_polls_independently() {
        let failing = Arc::new(ScriptedSink::with_inputs([Err("unavailable".to_string())]));
        let healthy = Arc::new(ScriptedSink::with_inputs([Ok(Some(0x12))]));
        let (config_tx, executors_tx, mut ctl_rx, state, cancel) = setup(failing);
        state.reconcile_shared([
            DisplayId("shared".to_string()),
            DisplayId("healthy".to_string()),
        ]);
        let mut config = (**config_tx.borrow()).clone();
        let healthy_config = config.displays["shared"].clone();
        config
            .displays
            .insert("healthy".to_string(), healthy_config);
        config_tx.send_replace(Arc::new(config));
        let mut executors = (*executors_tx.borrow()).as_ref().clone();
        executors.insert(
            DisplayId("healthy".to_string()),
            healthy as Arc<dyn CommandSink>,
        );
        executors_tx.send_replace(Arc::new(executors));
        tokio::task::yield_now().await;
        tick().await;
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { display }) if display == DisplayId("healthy".to_string())
        ));
        assert!(state.snapshot()[&DisplayId("shared".to_string())].owned);
        assert!(!state.snapshot()[&DisplayId("healthy".to_string())].owned);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn poll_interval_change_via_config_reload_applies() {
        let sink = Arc::new(ScriptedSink::with_inputs([Ok(Some(0x11)), Ok(Some(0x11))]));
        let (config_tx, _executors_tx, _ctl_rx, _state, cancel) = setup(sink.clone());
        tick().await;
        assert_eq!(sink.reads(), 1);
        let mut config = (**config_tx.borrow()).clone();
        config.coordination.poll_interval = Duration::from_secs(3);
        config_tx.send_replace(Arc::new(config));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(sink.reads(), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(sink.reads(), 2);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_error_holds_false_and_does_not_poke() {
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Err("skipped: command holds panel lock".to_string()),
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup(sink);
        tick().await;
        let _ = ctl_rx.recv().await;
        tick().await;
        assert!(!state.snapshot()[&DisplayId("shared".to_string())].owned);
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_success_logs_ok_and_resets_failures() {
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Err("transient".to_string()),
            Ok(Some(0x12)),
        ]));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink);
        tick().await;
        tick().await;
        tick().await;
        assert_eq!(
            state.snapshot()[&DisplayId("shared".to_string())].consecutive_failures,
            0
        );
        cancel.cancel();
        let events = captured_events(
            [Ok(Some(0x12)), Err("transient".to_string()), Ok(Some(0x12))],
            3,
        )
        .await;
        assert_eq!(count_event(&events, "coord_poll_ok"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_failures_use_named_thirty_second_interval() {
        assert_eq!(COORD_POLL_FAILING_LOG_INTERVAL, Duration::from_secs(30));
        let failures = || std::iter::repeat_with(|| Err("transient".to_string()));
        let first_window = captured_events(failures().take(5), 5).await;
        assert_eq!(count_event(&first_window, "coord_poll_failing"), 1);
        let full_window = captured_events(failures().take(12), 12).await;
        assert_eq!(count_event(&full_window, "coord_poll_failing"), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn poller_never_holds_cache_lock_across_read() {
        let sink = Arc::new(ScriptedSink::with_inputs([Ok(Some(0x11))]));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink.clone());
        sink.probe_cache(state);
        tokio::time::timeout(Duration::from_secs(7), tick())
            .await
            .expect("reader can acquire the cache lock");
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn poll_during_generation_swap_skips_empty_executor_window_then_recovers() {
        let sink = Arc::new(ScriptedSink::with_inputs([Ok(Some(0x11))]));
        let (_config_tx, executors_tx, _ctl_rx, _state, cancel) = setup(sink.clone());
        executors_tx.send_replace(Arc::new(HashMap::new()));
        tick().await;
        assert_eq!(sink.reads(), 0);
        let display = DisplayId("shared".to_string());
        executors_tx.send_replace(Arc::new(HashMap::from([(
            display,
            sink.clone() as Arc<dyn CommandSink>,
        )])));
        tick().await;
        assert_eq!(sink.reads(), 1);
        cancel.cancel();
    }

    struct EventVisitor {
        event: Option<String>,
    }

    impl Visit for EventVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event" {
                self.event = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for EventCapture {
        fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
            let mut visitor = EventVisitor { event: None };
            event.record(&mut visitor);
            if let Some(event) = visitor.event {
                self.0.lock().unwrap().push(event);
            }
        }
    }

    fn count_event(events: &[String], expected: &str) -> usize {
        events
            .iter()
            .filter(|event| event.as_str() == expected)
            .count()
    }

    async fn captured_events(
        inputs: impl IntoIterator<Item = Result<Option<u8>, String>>,
        ticks: u8,
    ) -> Vec<String> {
        let capture = EventCapture::default();
        let sink = Arc::new(ScriptedSink::with_inputs(inputs));
        let events = capture.0.clone();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(capture));
        let (config_tx, executors_tx, _ctl_rx, _state, cancel) = setup(sink);
        for _ in 0..ticks {
            tick().await;
        }
        cancel.cancel();
        tokio::task::yield_now().await;
        drop((config_tx, executors_tx));
        events.lock().unwrap().clone()
    }

    #[tokio::test(start_paused = true)]
    async fn emits_literal_coord_poll_ok_event_field() {
        assert!(
            captured_events([Ok(Some(0x11))], 1)
                .await
                .contains(&"coord_poll_ok".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn emits_literal_coord_poll_failing_event_field() {
        assert!(
            captured_events([Err("failed".to_string()), Err("failed".to_string())], 2)
                .await
                .contains(&"coord_poll_failing".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn emits_literal_coord_ownership_changed_event_field() {
        assert!(
            captured_events([Ok(Some(0x12))], 1)
                .await
                .contains(&"coord_ownership_changed".to_string())
        );
    }
}
