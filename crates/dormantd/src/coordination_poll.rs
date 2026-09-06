//! Periodic shared-display ownership polling.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dormant_core::config::{Config, DisplayScope, defaults};
use dormant_core::coordination::{
    COORD_POLL_FAILING_LOG_INTERVAL, CoordinationHandle, InputCodeAliases, InputSourceObservation,
};
use dormant_core::rules::{ControlMsg, DaemonEvent};
use dormant_core::traits::CommandSink;
use dormant_core::types::DisplayId;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::direct_switch::DirectSwitchHandle;

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
    /// Direct switch handle for firing post-hoc observed-loss hooks.
    /// `None` in tests that don't need hook firing.
    pub direct_switch: Option<Arc<DirectSwitchHandle>>,
}

/// Spawn the shared-display ownership poller.
#[must_use]
pub fn spawn(deps: CoordinationPollDeps) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(deps))
}

struct ReprobeState {
    last_attempt: Instant,
    next_interval: Duration,
}

async fn run(mut deps: CoordinationPollDeps) {
    let mut interval = new_interval(deps.config_rx.borrow().coordination.poll_interval);
    let mut last_failing_log = HashMap::new();
    let mut last_state_read: HashMap<DisplayId, Instant> = HashMap::new();
    let mut reprobe_state: HashMap<DisplayId, ReprobeState> = HashMap::new();
    loop {
        tokio::select! {
            () = deps.cancel.cancelled() => break,
            changed = deps.config_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                interval = new_interval(deps.config_rx.borrow().coordination.poll_interval);
            }
            _ = interval.tick() => poll_once(&deps, &mut last_failing_log, &mut last_state_read, &mut reprobe_state).await,
        }
    }
}

fn new_interval(period: Duration) -> tokio::time::Interval {
    let period = period.max(Duration::from_millis(1));
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

async fn maybe_reprobe(
    display_id: &DisplayId,
    executor: &dyn CommandSink,
    config: &Config,
    failures: u32,
    reprobe_state: &mut HashMap<DisplayId, ReprobeState>,
) {
    let coordination = &config.coordination;
    if failures < coordination.reprobe_failure_threshold {
        return;
    }
    let now = Instant::now();
    let floor = coordination.reprobe_interval;
    let current_interval = reprobe_state
        .get(display_id)
        .map_or(floor, |state| state.next_interval.max(floor));
    if reprobe_state
        .get(display_id)
        .is_some_and(|state| now.duration_since(state.last_attempt) < current_interval)
    {
        return;
    }

    tracing::warn!(
        event = "coord_poll_reprobe_attempt",
        display = %display_id,
        consecutive_failures = failures,
        interval = ?current_interval,
    );
    match executor.reprobe().await {
        Ok(()) => tracing::info!(
            event = "coord_poll_reprobe_ok",
            display = %display_id,
            interval = ?current_interval,
        ),
        Err(error) => tracing::warn!(
            event = "coord_poll_reprobe_failed",
            display = %display_id,
            interval = ?current_interval,
            error = %error,
        ),
    }
    let next_interval = current_interval
        .checked_mul(2)
        .unwrap_or(defaults::COORDINATION_REPROBE_MAX_INTERVAL)
        .min(defaults::COORDINATION_REPROBE_MAX_INTERVAL);
    reprobe_state.insert(
        display_id.clone(),
        ReprobeState {
            last_attempt: now,
            next_interval,
        },
    );
}

#[allow(clippy::too_many_lines)]
async fn poll_once(
    deps: &CoordinationPollDeps,
    last_failing_log: &mut HashMap<DisplayId, Instant>,
    last_state_read: &mut HashMap<DisplayId, Instant>,
    reprobe_state: &mut HashMap<DisplayId, ReprobeState>,
) {
    let executors = deps.executors_rx.borrow().clone();
    // Reload intentionally publishes this sentinel while an old generation tears down.
    if executors.is_empty() {
        return;
    }

    let config = deps.config_rx.borrow().clone();
    let state_poll_interval = config.coordination.effective_state_poll_interval();
    let now = Instant::now();
    // Snapshot once per tick: displays not due for a state read preserve their
    // last recorded panel_state, so `record_input_observation` rewrites the
    // same value and the DisplaySnapshot field stays stable between state
    // reads.
    let prior_panel_state = deps.state.snapshot();
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

        let aliases = InputCodeAliases {
            local_read: expected,
            local_write: display_config.shared_input_write_code.unwrap_or(expected),
            peer_read: display_config.shared_peer_input_code,
            peer_write: display_config.shared_peer_input_write_code,
        };

        // Ownership arbitration (VCP 0x60) runs every tick; panel-state cosmetics
        // (brightness/power) refresh only at the slower state_poll cadence to
        // cut per-transaction i2c traffic on cached-fd NVIDIA nodes. Hardware
        // reads may wait on controller locks; mutate the verdict cache only
        // after the input read (and, on a due tick, the state read) completes.
        let input = executor.read_input_source_sampled().await;
        let due = last_state_read
            .get(&display_id)
            .is_none_or(|last| now.duration_since(*last) >= state_poll_interval);
        let panel_state = if due {
            let panel_state = executor.read_state_sampled().await;
            if panel_state.is_some() {
                last_state_read.insert(display_id.clone(), now);
            }
            panel_state
        } else {
            prior_panel_state
                .get(&display_id)
                .and_then(|record| record.panel_state.clone())
        };
        if let Ok(Some(observed)) = input {
            // Validate against the configured code set (issue #138 part B).
            // A read that decodes to a value which is neither the local nor
            // peer input code is not a meaningful observation — concurrent
            // DDC traffic garbles the byte stream, and the resulting code
            // cannot be distinguished from the operator selecting an OSD
            // input. Treat it as a failed observation for ownership only:
            // hold the last verdict rather than feeding it through the
            // debounce, but do not re-probe a controller that did respond.
            let classification = aliases.classify(observed);
            if matches!(classification, InputSourceObservation::Unknown(_)) {
                deps.state.record_failure(&display_id);
                continue; // `for` loop — skip the rest of this display's tick
            }

            let before = deps.state.snapshot();
            let outcome = deps.state.record_input_observation(
                &display_id,
                observed,
                &aliases,
                config.coordination.loss_confirmations,
                panel_state,
            );
            let previous = before.get(&display_id);
            if previous.is_some_and(|record| {
                !record.has_successful_input_read || record.consecutive_failures > 0
            }) {
                tracing::info!(event = "coord_poll_ok", display = %display_id);
            }
            last_failing_log.remove(&display_id);
            if let Some(state) = reprobe_state.get_mut(&display_id) {
                state.next_interval = config.coordination.reprobe_interval;
            }
            // Observability for the issue #134 garbled-read path: a successful
            // but inconsistent `0x60` reading (cross-machine DDC traffic) sails
            // through the existing failure-counter path without a log line, so
            // emit a literal anchor whenever consecutive successful observations
            // disagree — operators have no other signal that the bus is dirty.
            if let Some(previous_code) = outcome.disagreement_with {
                tracing::warn!(
                    event = "coord_poll_disagreement",
                    display = %display_id,
                    previous_code,
                    observed,
                );
            }
            // A potential ownership loss is held pending further confirmations
            // (issue #134). Surfacing the deferred count lets operators see the
            // debounce in flight rather than mistaking the silence for a stall.
            if let Some(pending_count) = outcome.deferred_loss_count {
                tracing::info!(
                    event = "coord_ownership_loss_deferred",
                    display = %display_id,
                    pending_count,
                    observed,
                );
            }
            // A potential ownership gain is held pending further confirmations
            // (symmetric debounce). Surfacing the deferred count mirrors the
            // loss-deferred log so operators can see the gain in flight.
            if let Some(pending_count) = outcome.deferred_gain_count {
                tracing::info!(
                    event = "coord_ownership_gain_deferred",
                    display = %display_id,
                    pending_count,
                    observed,
                );
            }
            if let Some(previous_owned) = outcome.committed_prior_owned {
                let owned = matches!(aliases.classify(observed), InputSourceObservation::Local);
                tracing::info!(event = "coord_ownership_changed", display = %display_id, previous_owned, owned);
                // Feed ownership to the rules engine first so it wakes the
                // display before consumers see the event.
                let _ = deps
                    .ctl_tx
                    .send(ControlMsg::OwnershipPoll {
                        display: display_id.clone(),
                    })
                    .await;
                // Emit ownership event for debounced poll transition.
                let _ = deps
                    .ctl_tx
                    .send(ControlMsg::PublishDaemonEvent(DaemonEvent::Ownership {
                        display: display_id.clone(),
                        owned,
                        observed_input_code: Some(observed),
                        written_code: None,
                        cause: "poll".to_string(),
                        verified: None,
                        degraded: false,
                    }))
                    .await;
                // Post-hoc observed-loss hook: fires only on a committed
                // LOSS (previous_owned == true).  Gain emits no acquire
                // hook — the poll has no write authority.
                if previous_owned && let Some(ref ds) = deps.direct_switch {
                    ds.notify_observed_loss(&display_id).await;
                }
            }
        } else {
            deps.state.record_failure(&display_id);
            let failures = deps
                .state
                .snapshot()
                .get(&display_id)
                .map_or(0, |record| record.consecutive_failures);
            maybe_reprobe(
                &display_id,
                executor.as_ref(),
                &config,
                failures,
                reprobe_state,
            )
            .await;
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
    use super::{CoordinationPollDeps, poll_once, spawn};
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
    use dormant_core::traits::{CommandSink, PanelState, PowerState};
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
        state_reads: Mutex<u32>,
        reprobes: Mutex<u32>,
        cache_probe: Mutex<Option<CoordinationHandle>>,
    }

    impl ScriptedSink {
        fn with_inputs(inputs: impl IntoIterator<Item = Result<Option<u8>, String>>) -> Self {
            Self {
                inputs: Mutex::new(inputs.into_iter().collect()),
                ..Self::default()
            }
        }

        fn with_inputs_and_states(
            inputs: impl IntoIterator<Item = Result<Option<u8>, String>>,
            states: impl IntoIterator<Item = Option<PanelState>>,
        ) -> Self {
            Self {
                inputs: Mutex::new(inputs.into_iter().collect()),
                states: Mutex::new(states.into_iter().collect()),
                ..Self::default()
            }
        }

        fn reads(&self) -> u32 {
            *self.reads.lock().unwrap()
        }

        fn state_reads(&self) -> u32 {
            *self.state_reads.lock().unwrap()
        }

        fn reprobes(&self) -> u32 {
            *self.reprobes.lock().unwrap()
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

        async fn reprobe(&self) -> Result<(), String> {
            *self.reprobes.lock().unwrap() += 1;
            Ok(())
        }

        async fn read_state_sampled(&self) -> Option<PanelState> {
            *self.state_reads.lock().unwrap() += 1;
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
                shared_input_write_code: None,
                shared_peer_input_code: Some(0x12),
                shared_peer_input_write_code: None,
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
                hooks: dormant_core::config::HookSlots::default(),
                power_off_opt_in: false,
                compositor_output: None,
                sampling: None,
            },
        );
        Config {
            coordination: CoordinationConfig {
                poll_interval: Duration::from_secs(6),
                state_poll_interval: Some(Duration::from_secs(30)),
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
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        }
    }

    /// A `Config` where the shared display has `shared_peer_input_code = Some(peer)`.
    fn config_with_peer_read(peer: u8) -> Config {
        let mut cfg = config();
        if let Some(dc) = cfg.displays.get_mut("shared") {
            dc.shared_peer_input_code = Some(peer);
        }
        cfg
    }

    /// Like [`setup`] but accepts an explicit [`Config`].
    fn setup_with_config(cfg: Config, sink: Arc<ScriptedSink>) -> TestHarness {
        let (config_tx, config_rx) = watch::channel(Arc::new(cfg));
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
            direct_switch: None,
        });
        (config_tx, executors_tx, ctl_rx, state, cancel)
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
            direct_switch: None,
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
        // Default `loss_confirmations = 3` requires three agreeing "not mine"
        // readings before the verdict commits — issue #134 debounce.
        // peer_read = 0x12 configured so 0x12 is a known peer code (unknown
        // codes are treated as transport failures — issue #138 Fix B).
        let cfg = config_with_peer_read(0x12);
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, _state, cancel) = setup_with_config(cfg, sink);
        tick().await;
        tick().await;
        tick().await;
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { .. })
        ));
        // Drain the trailing PublishDaemonEvent(Ownership).
        assert!(matches!(
            ctl_rx.try_recv(),
            Ok(ControlMsg::PublishDaemonEvent(
                dormant_core::rules::DaemonEvent::Ownership { .. }
            ))
        ));
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn second_same_verdict_success_does_not_poke_again() {
        // First transition reads as committed; second transition's already-stable
        // not-mine reading must NOT re-fire (test the "stays false" half).
        // peer_read = 0x12 configured (unknown codes treated as failures — Fix B).
        let cfg = config_with_peer_read(0x12);
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, _state, cancel) = setup_with_config(cfg, sink);
        tick().await;
        tick().await;
        tick().await; // third tick: loss confirmed, OwnershipPoll + PublishDaemonEvent sent
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { .. })
        ));
        // Drain the trailing PublishDaemonEvent before the next tick.
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::PublishDaemonEvent(
                dormant_core::rules::DaemonEvent::Ownership { .. }
            ))
        ));
        tick().await; // fourth tick: already not owned, no further poke
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn each_shared_display_polls_independently() {
        let failing = Arc::new(ScriptedSink::with_inputs([Err("unavailable".to_string())]));
        let healthy = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
        ]));
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
        tick().await;
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
        // Three not-mine readings commit a loss; the error then holds it
        // through the existing hold-last-verdict path. No OwnershipPoll re-poke
        // after the loss commit, no extra pokes across the failure.
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Err("skipped: command holds panel lock".to_string()),
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup(sink);
        tick().await;
        tick().await;
        tick().await;
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { .. })
        ));
        // Drain the trailing PublishDaemonEvent before the error tick.
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::PublishDaemonEvent(
                dormant_core::rules::DaemonEvent::Ownership { .. }
            ))
        ));
        tick().await; // error after loss commit — verdict held, no poke
        assert!(!state.snapshot()[&DisplayId("shared".to_string())].owned);
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_success_logs_ok_and_resets_failures() {
        // loss_confirmations default 3: a stray failure mid-sequence never
        // commits a loss. The recovery tick still emits coord_poll_ok because
        // it lands after a failure.
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Err("transient".to_string()),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
        ]));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink);
        tick().await;
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
    async fn sustained_failures_trigger_one_reprobe_then_recover() {
        let capture = EventCapture::default();
        let events = capture.0.clone();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(capture));
        let sink = Arc::new(ScriptedSink::with_inputs([
            Err("hotplugged".to_string()),
            Err("hotplugged".to_string()),
            Err("hotplugged".to_string()),
            Ok(Some(0x11)),
        ]));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink.clone());
        for _ in 0..4 {
            tick().await;
        }

        assert_eq!(sink.reprobes(), 1);
        assert_eq!(
            state.snapshot()[&DisplayId("shared".to_string())].consecutive_failures,
            0
        );
        assert_eq!(count_event(&events.lock().unwrap(), "coord_poll_ok"), 1);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn reprobe_waits_for_configured_failure_threshold() {
        let mut cfg = config();
        cfg.coordination.reprobe_failure_threshold = 5;
        let sink = Arc::new(ScriptedSink::with_inputs(
            std::iter::repeat_with(|| Err("unreachable".to_string())).take(5),
        ));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) =
            setup_with_config(cfg, sink.clone());

        for _ in 0..4 {
            tick().await;
        }
        assert_eq!(sink.reprobes(), 0);
        assert_eq!(
            state.snapshot()[&DisplayId("shared".to_string())].consecutive_failures,
            4
        );

        tick().await;
        assert_eq!(sink.reprobes(), 1);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn dead_panel_reprobe_uses_bounded_backoff() {
        let sink = Arc::new(ScriptedSink::with_inputs(
            std::iter::repeat_with(|| Err("unreachable".to_string())).take(40),
        ));
        let (_config_tx, _executors_tx, _ctl_rx, _state, cancel) = setup(sink.clone());
        for _ in 0..40 {
            tick().await;
        }

        // At 6s polls, attempts land at 18s, 78s, and 198s (30s → 60s →
        // 120s), rather than every 30s forever.
        assert_eq!(sink.reprobes(), 3);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn successful_read_resets_reprobe_backoff_to_floor() {
        let sink = Arc::new(ScriptedSink::with_inputs([
            Err("hotplugged".to_string()),
            Err("hotplugged".to_string()),
            Err("hotplugged".to_string()),
            Ok(Some(0x11)),
            Err("hotplugged".to_string()),
            Err("hotplugged".to_string()),
            Err("hotplugged".to_string()),
            Ok(Some(0x11)),
        ]));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink.clone());
        for _ in 0..8 {
            tick().await;
        }

        assert_eq!(sink.reprobes(), 1);
        assert_eq!(
            state.snapshot()[&DisplayId("shared".to_string())].consecutive_failures,
            0
        );
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_panel_preserves_reprobe_interval_gate_across_recovery() {
        let sink = Arc::new(ScriptedSink::with_inputs((0..48).map(|tick| {
            if tick % 4 == 3 {
                Ok(Some(0x11))
            } else {
                Err("hotplugged".to_string())
            }
        })));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink.clone());
        let mut failures = Vec::new();
        for _ in 0..48 {
            tick().await;
            failures.push(state.snapshot()[&DisplayId("shared".to_string())].consecutive_failures);
        }

        assert_eq!(sink.reads(), 48);
        assert_eq!(sink.reprobes(), 6);
        assert_eq!(&failures[..4], &[1, 2, 3, 0]);
        assert!(
            failures
                .as_chunks::<4>()
                .0
                .iter()
                .all(|chunk| chunk == &[1, 2, 3, 0])
        );
        cancel.cancel();
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

    #[tokio::test(start_paused = true)]
    async fn state_read_skipped_until_state_poll_interval_elapses() {
        // poll_interval = 6s, state_poll_interval = 30s. VCP 0x60 (input) is read
        // every tick; panel-state reads happen on the first tick and again once
        // 30s have elapsed since the last state read.
        let panel = PanelState {
            power: Some(PowerState::On),
            brightness: Some(50),
        };
        let sink = Arc::new(ScriptedSink::with_inputs_and_states(
            std::iter::repeat_n(Ok(Some(0x11)), 6),
            [Some(panel.clone()), Some(panel.clone())],
        ));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink.clone());
        let shared = DisplayId("shared".to_string());
        tick().await; // t=6s: first tick, due
        assert_eq!(sink.reads(), 1);
        assert_eq!(sink.state_reads(), 1);
        assert_eq!(state.snapshot()[&shared].panel_state, Some(panel.clone()));
        // t=12,18,24,30s: state_poll_interval (30s) not yet elapsed since t=6s.
        for _ in 0..4 {
            tick().await;
        }
        assert_eq!(sink.reads(), 5);
        assert_eq!(
            sink.state_reads(),
            1,
            "state read skipped below state_poll_interval"
        );
        // Non-due ticks preserve the last panel_state from the verdict-cache
        // snapshot — it does NOT reset to None between state reads.
        assert_eq!(
            state.snapshot()[&shared].panel_state,
            Some(panel.clone()),
            "panel_state preserved across non-due ticks"
        );
        // t=36s: 36s - 6s = 30s >= state_poll_interval → due again.
        tick().await;
        assert_eq!(sink.reads(), 6, "0x60 read every tick");
        assert_eq!(
            sink.state_reads(),
            2,
            "state read after state_poll_interval elapsed"
        );
        // Ownership is fed every tick: six successful 0x60 reads, all matching
        // the expected 0x11, so the display stays owned with no failures.
        let record = &state.snapshot()[&shared];
        assert!(record.owned);
        assert!(record.has_successful_input_read);
        assert_eq!(record.consecutive_failures, 0);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_state_read_retries_on_the_next_poll_tick() {
        let panel = PanelState {
            power: Some(PowerState::On),
            brightness: Some(50),
        };
        let sink = Arc::new(ScriptedSink::with_inputs_and_states(
            [Ok(Some(0x11)), Ok(Some(0x11))],
            [None, Some(panel.clone())],
        ));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) = setup(sink.clone());
        let shared = DisplayId("shared".to_string());

        tick().await;
        assert_eq!(sink.state_reads(), 1, "first due state read fails");
        assert_eq!(state.snapshot()[&shared].panel_state, None);

        tick().await;
        assert_eq!(
            sink.state_reads(),
            2,
            "a failed state read must retry on the next poll tick"
        );
        assert_eq!(state.snapshot()[&shared].panel_state, Some(panel));
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
        let (config_tx, config_rx) = watch::channel(Arc::new(config()));
        let display = DisplayId("shared".to_string());
        let executors = HashMap::from([(display.clone(), sink as Arc<dyn CommandSink>)]);
        let (executors_tx, executors_rx) = watch::channel(Arc::new(executors));
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let deps = CoordinationPollDeps {
            config_rx,
            ctl_tx,
            executors_rx,
            state: CoordinationHandle::new([display]),
            cancel: CancellationToken::new(),
            direct_switch: None,
        };
        let mut last_failing_log = HashMap::new();
        let mut last_state_read = HashMap::new();
        let mut reprobe_state = HashMap::new();
        for _ in 0..ticks {
            tokio::time::advance(Duration::from_secs(6)).await;
            poll_once(
                &deps,
                &mut last_failing_log,
                &mut last_state_read,
                &mut reprobe_state,
            )
            .await;
        }
        drop((config_tx, executors_tx));
        events.lock().unwrap().clone()
    }

    /// Like [`captured_events`] but with an explicit [`Config`] for tests
    /// that need configured peer codes.
    async fn captured_events_with_config(
        cfg: Config,
        inputs: impl IntoIterator<Item = Result<Option<u8>, String>>,
        ticks: u8,
    ) -> Vec<String> {
        let capture = EventCapture::default();
        let sink = Arc::new(ScriptedSink::with_inputs(inputs));
        let events = capture.0.clone();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(capture));
        let (config_tx, config_rx) = watch::channel(Arc::new(cfg));
        let display = DisplayId("shared".to_string());
        let executors = HashMap::from([(display.clone(), sink as Arc<dyn CommandSink>)]);
        let (executors_tx, executors_rx) = watch::channel(Arc::new(executors));
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let deps = CoordinationPollDeps {
            config_rx,
            ctl_tx,
            executors_rx,
            state: CoordinationHandle::new([display]),
            cancel: CancellationToken::new(),
            direct_switch: None,
        };
        let mut last_failing_log = HashMap::new();
        let mut last_state_read = HashMap::new();
        let mut reprobe_state = HashMap::new();
        for _ in 0..ticks {
            tokio::time::advance(Duration::from_secs(6)).await;
            poll_once(
                &deps,
                &mut last_failing_log,
                &mut last_state_read,
                &mut reprobe_state,
            )
            .await;
        }
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
        // Issue #134: `loss_confirmations = 3` requires three agreeing
        // not-mine readings before the verdict commits and the literal
        // `coord_ownership_changed` event fires.
        assert!(
            captured_events([Ok(Some(0x12)), Ok(Some(0x12)), Ok(Some(0x12))], 3,)
                .await
                .contains(&"coord_ownership_changed".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn emits_literal_coord_poll_disagreement_event_field() {
        // Two consecutive successful observations with different known codes
        // disagree; the literal `coord_poll_disagreement` event must fire so
        // the operator sees the bus returning inconsistent values.
        // peer_read = 0x12 makes 0x12 a known not-mine code so the disagreement
        // path engages (unknown codes are treated as transport failures —
        // issue #138 Fix B).
        assert!(
            captured_events_with_config(
                config_with_peer_read(0x12),
                [Ok(Some(0x11)), Ok(Some(0x12))],
                2,
            )
            .await
            .contains(&"coord_poll_disagreement".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn emits_literal_coord_ownership_loss_deferred_event_field() {
        // One stray not-mine reading under `loss_confirmations = 3` is
        // deferred; the literal `coord_ownership_loss_deferred` event must
        // surface the pending count for operator visibility.
        // peer_read = 0x12 configured so the not-mine code enters the
        // debounce (unknown codes are treated as failures — Fix B).
        assert!(
            captured_events_with_config(
                config_with_peer_read(0x12),
                [Ok(Some(0x11)), Ok(Some(0x12))],
                2,
            )
            .await
            .contains(&"coord_ownership_loss_deferred".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stray_not_mine_reading_does_not_poke() {
        // Issue #134 anchor: a single not-mine reading must NOT commit a
        // loss and must NOT trigger a control-channel poke, even after two
        // prior owned readings. This is the exact `owned, owned, WRONG`
        // shape from the live journal.
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x11)),
            Ok(Some(0x11)),
            Ok(Some(0x99)),
            Ok(Some(0x11)),
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup(sink);
        for _ in 0..4 {
            tick().await;
        }
        assert!(
            ctl_rx.try_recv().is_err(),
            "no OwnershipPoll must be sent while verdict stays owned"
        );
        assert!(state.snapshot()[&DisplayId("shared".to_string())].owned);
        cancel.cancel();
    }

    /// Fix B (#138): an unknown code that is neither local nor peer is treated
    /// as a transport failure — verdict held, debounce untouched.
    #[tokio::test(start_paused = true)]
    async fn unknown_code_treated_as_failure_verdict_held() {
        // peer_read = 0x12 configured; 0x13 is unknown.
        let cfg = config_with_peer_read(0x12);
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x11)), // tick 1: owned, mine
            Ok(Some(0x13)), // tick 2: Unknown → failure, verdict held
            Ok(Some(0x11)), // tick 3: mine again, consecutive_failures reset
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup_with_config(cfg, sink);
        let events_layer = EventCapture::default();
        let events = events_layer.0.clone();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(events_layer));
        for _ in 0..3 {
            tick().await;
        }
        // Verdict must stay owned — the unknown code did not enter the debounce.
        assert!(state.snapshot()[&DisplayId("shared".to_string())].owned);
        assert!(ctl_rx.try_recv().is_err(), "no OwnershipPoll expected");
        let captured = events.lock().unwrap().clone();
        // No ownership-change, no loss_deferred, no disagreement — the unknown
        // code was a transport failure, not an observation.
        assert!(
            !captured
                .iter()
                .any(|event| event == "coord_ownership_loss_deferred"),
            "unknown code must not trigger loss deferred, got {captured:?}"
        );
        assert!(
            !captured
                .iter()
                .any(|event| event == "coord_ownership_changed"),
            "unknown code must not trigger ownership change, got {captured:?}"
        );
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn garbled_reads_hold_verdict_without_reprobe() {
        let cfg = config_with_peer_read(0x12);
        let sink = Arc::new(ScriptedSink::with_inputs(
            std::iter::repeat_with(|| Ok(Some(0x99))).take(12),
        ));
        let (_config_tx, _executors_tx, _ctl_rx, state, cancel) =
            setup_with_config(cfg, sink.clone());
        for _ in 0..12 {
            tick().await;
        }

        let record = state.snapshot()[&DisplayId("shared".to_string())].clone();
        assert_eq!(sink.reads(), 12);
        assert_eq!(sink.reprobes(), 0);
        assert!(record.owned);
        assert_eq!(record.consecutive_failures, 12);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn disagreeing_not_mine_reads_hold_verdict_and_emit_disagreement() {
        // Two different known not-mine codes in a row must NOT commit a loss
        // and must surface a disagreement signal — extending the hold-last-verdict
        // path to successful-but-inconsistent reads (issue #134 §3.4 verdict
        // table extension). The disagreeing reading resets the pending counter
        // so the next agreeing reading stays below the 3-confirmation
        // threshold.
        //
        // Configure peer_read = 0x12 so that codes 0x12 and 0x13 can still
        // be distinguished by the debounce: 0x12 is Peer, 0x13 is Unknown
        // (treated as failure by issue #138 Fix B — validation against
        // configured code set).
        let cfg = config_with_peer_read(0x12);
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x11)), // tick 1: owned, mine
            Ok(Some(0x12)), // tick 2: not-mine peer, pending=1
            Ok(Some(0x13)), // tick 3: Unknown, treated as failure (hold verdict)
            Ok(Some(0x12)), // tick 4: not-mine peer, pending=2 (still under N=3)
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup_with_config(cfg, sink);
        let events_layer = EventCapture::default();
        let events = events_layer.0.clone();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(events_layer));
        for _ in 0..4 {
            tick().await;
        }
        assert!(ctl_rx.try_recv().is_err(), "no loss commit expected");
        assert!(state.snapshot()[&DisplayId("shared".to_string())].owned);
        // Fix B (#138): Unknown codes (0x13) are treated as transport
        // failures, not as debounce observations — the pending transition
        // from tick 2 (peer code 0x12) survives the failure intact.  Tick 4
        // agrees with it (pending=2), still below confirmations=3.
        // The disagreement at tick 2 (0x11→0x12) fires correctly because
        // both codes are in the configured set.
        let captured = events.lock().unwrap().clone();
        assert!(
            captured
                .iter()
                .any(|event| event == "coord_poll_disagreement"),
            "disagreement must fire when 0x11→0x12 (both configured), got {captured:?}"
        );
        // No loss committed — tick 3 was a failure, and tick 4 only brings
        // pending to 2 (under the 3-confirmation threshold).
        assert!(
            !captured
                .iter()
                .any(|event| event == "coord_ownership_changed"),
            "no ownership change expected, got {captured:?}"
        );
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn three_agreeing_not_mine_readings_commit_loss_exactly_once() {
        // Issue #134 anchor (positive case): a real sustained input switch
        // (three agreeing not-mine readings) commits exactly one loss.
        // Configure peer_read = 0x12 so 0x12 is a known peer code — unknown
        // codes are treated as transport failures (issue #138 Fix B).
        let cfg = config_with_peer_read(0x12);
        let sink = Arc::new(ScriptedSink::with_inputs([
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
            Ok(Some(0x12)),
        ]));
        let (_config_tx, _executors_tx, mut ctl_rx, state, cancel) = setup_with_config(cfg, sink);
        tick().await;
        tick().await;
        tick().await; // third tick: loss confirmed, OwnershipPoll + PublishDaemonEvent sent
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::OwnershipPoll { .. })
        ));
        assert!(!state.snapshot()[&DisplayId("shared".to_string())].owned);
        // Drain the trailing PublishDaemonEvent before the next tick.
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ControlMsg::PublishDaemonEvent(
                dormant_core::rules::DaemonEvent::Ownership { .. }
            ))
        ));
        tick().await; // fourth tick: already not owned, no further poke
        assert!(ctl_rx.try_recv().is_err());
        cancel.cancel();
    }
}
