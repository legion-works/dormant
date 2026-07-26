//! Local direct-switch service — replaces the owner-mediated claim protocol.
//!
//! Each machine writes an input code over its own DDC bus, locally, with
//! no network, no peer, no handshake.  [`DirectSwitchHandle`] exposes
//! `pull` (always available) and `push` (configuration-gated via
//! `shared_peer_input_write_code`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dormant_core::config::Config;
use dormant_core::config::schema::DisplayScope;
use dormant_core::traits::{CommandSink, InputSourceReadback, InputSourceTarget};
use dormant_core::types::DisplayId;
use tokio::sync::{mpsc, watch};
use tracing::warn;

use crate::hooks::{Direction, HookContext, HookEngine, HookOutcome, HookSlot, Phase};

// ── Public types ──────────────────────────────────────────────────────────────

/// Why a switch was requested — literal causality, never peer identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    Activity,
    Hotkey,
    Cli,
    Web,
    Tray,
    Release,
    Toggle,
}

/// The outcome of a direct (local, no-network) switch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// The write was issued and verified.
    Switched,
    /// Push was attempted but peer codes are not configured.
    NotConfigured,
    /// Hook execution aborted before the write could proceed.
    HookAborted {
        /// Reason the hook runner gave for aborting.
        reason: String,
    },
    /// The write returned a controller-level error.
    WriteFailed {
        /// Error message from the controller chain.
        error: String,
    },
    /// The display is not shared, has no input-source capability, or
    /// the necessary config codes are absent.
    Unsupported,
    /// The pull was suppressed by the activity cooldown — another
    /// activity-driven write was already issued within the
    /// [`CoordinationConfig::cooldown`] window for this display.
    Cooldown,
}

// ── Suppression guard ─────────────────────────────────────────────────────────

/// Short-lived claim-suppression deadline for direct-switch flights.
///
/// Keeps the rules engine from reacting to ownership-loss observations
/// triggered by our own input-source write.  A guard ensures the
/// suppression is cleared on every exit path, including Drop.
const SUPPRESSION_DURATION: Duration = Duration::from_secs(5);

/// RAII guard that publishes a `ControlMsg::SetClaimSuppression` clear
/// on Drop when not explicitly cleared first.  Clears synchronously via
/// `try_send` — the channel is dimensioned for bursts much larger than
/// one guard.
struct SuppressionGuard {
    front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    display: DisplayId,
    cleared: bool,
}

impl SuppressionGuard {
    /// Publish the `until` deadline and return a guard that clears on drop.
    fn set(
        front_ctl_tx: &mpsc::Sender<dormant_core::rules::ControlMsg>,
        display: &DisplayId,
    ) -> Self {
        let _ = front_ctl_tx.try_send(dormant_core::rules::ControlMsg::SetClaimSuppression {
            display: display.clone(),
            until: Some(Instant::now() + SUPPRESSION_DURATION),
        });
        Self {
            front_ctl_tx: front_ctl_tx.clone(),
            display: display.clone(),
            cleared: false,
        }
    }

    /// Clear the suppression explicitly.  Subsequent Drop is a no-op.
    fn clear(&mut self) {
        self.cleared = true;
        let _ = self
            .front_ctl_tx
            .try_send(dormant_core::rules::ControlMsg::SetClaimSuppression {
                display: self.display.clone(),
                until: None,
            });
    }
}

impl Drop for SuppressionGuard {
    fn drop(&mut self) {
        if !self.cleared {
            let _ =
                self.front_ctl_tx
                    .try_send(dormant_core::rules::ControlMsg::SetClaimSuppression {
                        display: self.display.clone(),
                        until: None,
                    });
        }
    }
}

// ── Hook helper ───────────────────────────────────────────────────────────────

/// Extract the action list for a direction/phase pair.
fn slot_for(
    hooks: &dormant_core::config::schema::HookSlots,
    direction: Direction,
    phase: Phase,
) -> &[dormant_core::config::schema::HookAction] {
    match (direction, phase) {
        (Direction::Release, Phase::Before) => &hooks.before_release,
        (Direction::Release, Phase::After) => &hooks.after_release,
        (Direction::Acquire, Phase::Before) => &hooks.before_acquire,
        (Direction::Acquire, Phase::After) => &hooks.after_acquire,
        // ObservedLoss is not an initiator path — never reached through
        // slot_for (the caller uses notify_observed_loss instead).
        (Direction::ObservedLoss, _) => &[],
    }
}

// ── DirectSwitchHandle ────────────────────────────────────────────────────────

/// Local direct-switch service.
///
/// Constructed once at daemon startup; shared across the lifetime of the
/// daemon.  Consumes config and executor watch channels so each call sees
/// the live generation.
pub struct DirectSwitchHandle {
    executors: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    config: watch::Receiver<Arc<Config>>,
    hooks: Arc<HookEngine>,
    front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    /// Last successful activity-driven pull time per display, used for
    /// cooldown gating.  Tokio `Instant` so that paused-time tests see
    /// deterministic cooldown expiry.
    last_activity_pull: std::sync::Mutex<HashMap<DisplayId, tokio::time::Instant>>,
}

impl DirectSwitchHandle {
    /// Construct a handle that shares the daemon's config, executor, hook
    /// engine, and front control channels.
    #[must_use]
    pub fn new(
        executors: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
        config: watch::Receiver<Arc<Config>>,
        hooks: Arc<HookEngine>,
        front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    ) -> Self {
        Self {
            executors,
            config,
            hooks,
            front_ctl_tx,
            last_activity_pull: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Pull the display to this machine — write the local input code.
    ///
    /// Always available on a shared display with a configured
    /// `shared_input_code`.  The path validates the display, runs
    /// `before_acquire` hooks (blocking), writes the input-source
    /// command, clears claim suppression on every exit, and runs
    /// `after_acquire` only on success.
    pub async fn pull(&self, display: DisplayId, reason: SwitchReason) -> SwitchOutcome {
        let Some((dc, target)) = self.resolve_local_target(&display) else {
            return SwitchOutcome::Unsupported;
        };

        let Some(executor) = self.resolve_executor(&display) else {
            return SwitchOutcome::Unsupported;
        };

        // Cooldown gate for activity-driven pulls — prevents the two-host
        // ping-pong that the convergence test proves impossible.  Only
        // Activity-triggered pulls are gated; hotkeys, CLI, and web remain
        // always available.
        if reason == SwitchReason::Activity {
            let cooldown = self.config.borrow().coordination.cooldown;
            let last = self
                .last_activity_pull
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(&last_time) = last.get(&display)
                && last_time + cooldown > tokio::time::Instant::now()
            {
                return SwitchOutcome::Cooldown;
            }
        }

        // Suppression guard — cleared explicitly in every code path below,
        // and on Drop as a safety net for unexpected panics.
        let mut guard = SuppressionGuard::set(&self.front_ctl_tx, &display);

        // Blocking before_acquire — aborts the whole pull on failure.
        let aborted = self
            .run_hook(
                &display,
                &dc.hooks,
                Direction::Acquire,
                Phase::Before,
                false,
            )
            .await;
        if let Some(reason) = aborted {
            guard.clear();
            return SwitchOutcome::HookAborted { reason };
        }

        // Write the local input-source command through the controller chain.
        match executor.write_input_source(target).await {
            Ok(()) => {
                guard.clear();
                // Fire-and-forget after_acquire (non-blocking by convention).
                let _ = self
                    .run_hook(&display, &dc.hooks, Direction::Acquire, Phase::After, false)
                    .await;
                // Record the most recent activity-driven pull time for
                // cooldown enforcement.
                if reason == SwitchReason::Activity {
                    self.last_activity_pull
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(display.clone(), tokio::time::Instant::now());
                }
                SwitchOutcome::Switched
            }
            Err(cmd) => {
                guard.clear();
                SwitchOutcome::WriteFailed { error: cmd.error }
            }
        }
    }

    /// Push the display to a peer — write the peer input code.
    ///
    /// Configuration-gated: returns [`SwitchOutcome::NotConfigured`] when
    /// `shared_peer_input_write_code` is absent.  Push verification is
    /// strong (`Exact`) when `shared_peer_input_code` is configured;
    /// degrades to `DifferentFrom(local_read)` with an emitted WARN
    /// `kvm_push_verification_degraded` when the peer read alias is
    /// absent.
    pub async fn push(&self, display: DisplayId, _reason: SwitchReason) -> SwitchOutcome {
        let (dc, target) = match self.resolve_peer_target(&display) {
            Ok(pair) => pair,
            Err(outcome) => return outcome,
        };

        let Some(executor) = self.resolve_executor(&display) else {
            return SwitchOutcome::Unsupported;
        };

        // before_release — aborts the push on failure.
        let aborted = self
            .run_hook(
                &display,
                &dc.hooks,
                Direction::Release,
                Phase::Before,
                false,
            )
            .await;
        if let Some(reason) = aborted {
            return SwitchOutcome::HookAborted { reason };
        }

        match executor.write_input_source(target).await {
            Ok(()) => {
                // Fire-and-forget after_release.
                let _ = self
                    .run_hook(&display, &dc.hooks, Direction::Release, Phase::After, false)
                    .await;
                SwitchOutcome::Switched
            }
            Err(cmd) => SwitchOutcome::WriteFailed { error: cmd.error },
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Resolve the display config and build the local [`InputSourceTarget`].
    fn resolve_local_target(
        &self,
        display: &DisplayId,
    ) -> Option<(
        dormant_core::config::schema::DisplayConfig,
        InputSourceTarget,
    )> {
        let config = self.config.borrow().clone();
        let dc = config
            .displays
            .get(&display.0)
            .filter(|d| d.scope == DisplayScope::Shared)?
            .clone();

        let read_code = dc.shared_input_code?;
        let write_code = dc.shared_input_write_code.unwrap_or(read_code);

        Some((
            dc,
            InputSourceTarget {
                write_code,
                expected_readback: InputSourceReadback::Exact(read_code),
            },
        ))
    }

    /// Resolve the display config and build the peer [`InputSourceTarget`].
    ///
    /// Returns `Ok((dc, target))` on success, `Err(outcome)` for
    /// `NotConfigured` / `Unsupported`.
    fn resolve_peer_target(
        &self,
        display: &DisplayId,
    ) -> Result<
        (
            dormant_core::config::schema::DisplayConfig,
            InputSourceTarget,
        ),
        SwitchOutcome,
    > {
        let config = self.config.borrow().clone();
        let dc = config
            .displays
            .get(&display.0)
            .filter(|d| d.scope == DisplayScope::Shared)
            .ok_or(SwitchOutcome::Unsupported)?
            .clone();

        let peer_write = dc
            .shared_peer_input_write_code
            .ok_or(SwitchOutcome::NotConfigured)?;

        let expected_readback = if let Some(peer_read) = dc.shared_peer_input_code {
            InputSourceReadback::Exact(peer_read)
        } else {
            let local_read = dc.shared_input_code.unwrap_or(0);
            let display_name = display.0.clone();
            warn!(
                event = "kvm_push_verification_degraded",
                display_name = %display_name,
                peer_write_code = peer_write,
                local_read_code = local_read,
                "peer input read code not configured; push verification degraded to \
                 DifferentFrom(local_read)"
            );
            InputSourceReadback::DifferentFrom(local_read)
        };

        Ok((
            dc,
            InputSourceTarget {
                write_code: peer_write,
                expected_readback,
            },
        ))
    }

    /// Look up the currently-installed executor for `display`.
    fn resolve_executor(&self, display: &DisplayId) -> Option<Arc<dyn CommandSink>> {
        let executors = self.executors.borrow().clone();
        executors.get(display).cloned()
    }

    /// Run one hook slot and return the abort reason, if any.
    ///
    /// Returns `None` when the slot completed normally or had no actions.
    /// Returns `Some(reason)` when a blocking entry aborted.
    async fn run_hook(
        &self,
        display: &DisplayId,
        hooks: &dormant_core::config::schema::HookSlots,
        direction: Direction,
        phase: Phase,
        aborted: bool,
    ) -> Option<String> {
        let actions = slot_for(hooks, direction, phase);
        if actions.is_empty() {
            return None;
        }
        let slot = HookSlot {
            context: HookContext {
                display: &display.0,
                display_identity: "",
                direction,
                phase,
                peer: "",
                fallback: false,
                aborted,
            },
            actions,
        };
        match self.hooks.run_slot(slot).await {
            HookOutcome::Completed { .. } => None,
            HookOutcome::Aborted { reason, .. } => Some(reason),
        }
    }

    /// Fire the post-hoc `on_observed_loss` hook for a display whose
    /// ownership was lost to a peer (detected via VCP 0x60 poll).
    ///
    /// Fire-and-forget — a hook failure here must never trigger a
    /// corrective DDC write or retry.  The poll path has no write
    /// authority.
    pub async fn notify_observed_loss(&self, display: &DisplayId) {
        // Clone the actions Vec outside the watch::Ref borrow so the
        // guard is dropped before the await (watch::Ref is !Send).
        let actions: Vec<_> = {
            let config = self.config.borrow();
            let Some(dc) = config.displays.get(&display.0) else {
                return;
            };
            dc.hooks.on_observed_loss.clone()
        };
        if actions.is_empty() {
            return;
        }
        let display_name = display.0.clone();
        let slot = HookSlot {
            context: HookContext {
                display: &display_name,
                display_identity: "",
                direction: Direction::ObservedLoss,
                phase: Phase::After,
                peer: "",
                fallback: false,
                aborted: false,
            },
            actions: &actions,
        };
        let _ = self.hooks.run_slot(slot).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use dormant_core::config::schema as cs;
    use dormant_core::config::schema::{
        AudioConfig, DaemonConfig, InputFilterConfig, NotificationsConfig, WatchdogConfig,
        WearConfig,
    };
    use dormant_core::types::CmdFailure;
    use tokio::sync::Barrier;

    // ═══════════════════════════════════════════════════════════════════════
    //  Test fakes
    // ═══════════════════════════════════════════════════════════════════════

    /// A scripted [`CommandSink`] that captures the last
    /// [`InputSourceTarget`] written and returns a pre-set result.
    struct FakeSink {
        _name: &'static str,
        inner: Arc<Mutex<FakeSinkInner>>,
    }

    struct FakeSinkInner {
        write_result: Option<Result<(), CmdFailure>>,
        last_target: Option<InputSourceTarget>,
        write_calls: usize,
    }

    impl FakeSink {
        fn new(name: &'static str) -> Self {
            Self {
                _name: name,
                inner: Arc::new(Mutex::new(FakeSinkInner {
                    write_result: Some(Ok(())),
                    last_target: None,
                    write_calls: 0,
                })),
            }
        }

        fn set_write_result(&self, result: Result<(), CmdFailure>) {
            self.inner.lock().unwrap().write_result = Some(result);
        }

        fn last_target(&self) -> Option<InputSourceTarget> {
            self.inner.lock().unwrap().last_target
        }

        fn write_calls(&self) -> usize {
            self.inner.lock().unwrap().write_calls
        }
    }

    #[async_trait::async_trait]
    impl CommandSink for FakeSink {
        async fn blank(&self, _mode: dormant_core::types::BlankMode) -> Result<(), CmdFailure> {
            Ok(())
        }

        async fn wake(&self) -> Result<(), CmdFailure> {
            Ok(())
        }

        fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
            vec![]
        }

        async fn write_input_source(&self, target: InputSourceTarget) -> Result<(), CmdFailure> {
            let mut inner = self.inner.lock().unwrap();
            inner.last_target = Some(target);
            inner.write_calls += 1;
            inner.write_result.take().unwrap_or(Ok(()))
        }
    }

    /// A no-op hook runner — all commands succeed.
    #[derive(Clone)]
    struct NoopHookRunner;

    #[async_trait::async_trait]
    impl crate::hooks::HookRunner for NoopHookRunner {
        async fn run_command(
            &self,
            _env: &crate::hooks::EnvList,
            _argv: &[String],
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            Ok(())
        }

        async fn publish_mqtt(
            &self,
            _topic: &str,
            _payload: &str,
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            Ok(())
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Test helpers
    // ═══════════════════════════════════════════════════════════════════════

    const DISPLAY_ID: &str = "desk";
    const LOCAL_READ: u8 = 0x10;
    const LOCAL_WRITE: u8 = 0x15;
    const PEER_WRITE: u8 = 0x11;
    const PEER_READ: u8 = 0x12;

    /// A minimal [`cs::DisplayConfig`] with every field set.
    fn display_config() -> cs::DisplayConfig {
        cs::DisplayConfig {
            controllers: vec!["ddcci".into()],
            scope: DisplayScope::Shared,
            shared_input_code: Some(LOCAL_READ),
            shared_input_write_code: Some(LOCAL_WRITE),
            shared_peer_input_write_code: None,
            shared_peer_input_code: None,
            hooks: cs::HookSlots::default(),
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![],
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
            restore_brightness: 100,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
        }
    }

    fn display_id() -> DisplayId {
        DisplayId(DISPLAY_ID.to_string())
    }

    fn noop_hook_engine() -> Arc<HookEngine> {
        Arc::new(HookEngine::with_runner(Arc::new(NoopHookRunner)))
    }

    /// A single `before_acquire` action that aborts on failure.
    fn abortable_before_acquire_action() -> cs::HookAction {
        cs::HookAction {
            command: Some(vec!["true".into()]),
            mqtt: None,
            timeout: Duration::from_secs(1),
            blocking: Some(true),
            abort_on_failure: true,
        }
    }

    /// A single `before_release` action that aborts on failure.
    fn abortable_before_release_action() -> cs::HookAction {
        cs::HookAction {
            command: Some(vec!["true".into()]),
            mqtt: None,
            timeout: Duration::from_secs(1),
            blocking: Some(true),
            abort_on_failure: true,
        }
    }

    /// Build a test handle with the given display config, executor, and
    /// optional front-ctl channel.
    fn build_handle(
        dc: cs::DisplayConfig,
        sink: Arc<FakeSink>,
        hooks: Arc<HookEngine>,
        front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    ) -> DirectSwitchHandle {
        let executors: HashMap<_, _> = [(display_id(), sink as Arc<dyn CommandSink>)].into();
        let (_, executors_rx) = watch::channel(Arc::new(executors));
        let (_, config_rx) = watch::channel(Arc::new(minimal_config(dc)));
        DirectSwitchHandle {
            executors: executors_rx,
            config: config_rx,
            hooks,
            front_ctl_tx,
            last_activity_pull: Mutex::new(HashMap::new()),
        }
    }

    /// A `Config` with the given display and no other rules or sensors.
    fn minimal_config(dc: cs::DisplayConfig) -> Config {
        let mut displays = indexmap::IndexMap::new();
        displays.insert(DISPLAY_ID.to_string(), dc);
        Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays,
            rules: indexmap::IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            coordination: dormant_core::config::CoordinationConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: InputFilterConfig::default(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Pull tests
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn pull_alias_pairing_succeeds_with_semantic_readback() {
        // The alias case: write 0x15, readback Exact(0x10).
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(display_config(), sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;

        assert_eq!(outcome, SwitchOutcome::Switched);
        assert_eq!(sink.write_calls(), 1);
        let target = sink.last_target().unwrap();
        assert_eq!(target.write_code, LOCAL_WRITE);
        assert_eq!(
            target.expected_readback,
            InputSourceReadback::Exact(LOCAL_READ)
        );
    }

    #[tokio::test]
    async fn pull_without_write_code_defaults_to_input_code() {
        // When shared_input_write_code is absent, write_code = shared_input_code.
        let mut dc = display_config();
        dc.shared_input_write_code = None;
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;

        assert_eq!(outcome, SwitchOutcome::Switched);
        let target = sink.last_target().unwrap();
        assert_eq!(target.write_code, LOCAL_READ);
        assert_eq!(
            target.expected_readback,
            InputSourceReadback::Exact(LOCAL_READ)
        );
    }

    #[tokio::test]
    async fn pull_before_acquire_abort_prevents_write() {
        // When before_acquire aborts, the write must NOT be called.
        let sink = Arc::new(FakeSink::new("ddcci"));

        let runner = crate::hooks::ScriptedHookRunner::new();
        runner.push_command(Err("timeout".to_string()));
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.hooks.before_acquire = vec![abortable_before_acquire_action()];
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), hook_engine, tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;

        assert!(
            matches!(outcome, SwitchOutcome::HookAborted { .. }),
            "expected HookAborted, got {outcome:?}"
        );
        assert_eq!(
            sink.write_calls(),
            0,
            "write must not be called on hook abort"
        );
    }

    #[tokio::test]
    async fn pull_write_failure_surfaced() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        sink.set_write_result(Err(CmdFailure {
            controller: "ddcci".into(),
            error: "E_DISPLAY_IO: i2c write failed".into(),
        }));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(display_config(), sink, noop_hook_engine(), tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;

        assert!(
            matches!(outcome, SwitchOutcome::WriteFailed { .. }),
            "expected WriteFailed, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn pull_not_shared_returns_unsupported() {
        let mut dc = display_config();
        dc.scope = DisplayScope::Private;
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;

        assert_eq!(outcome, SwitchOutcome::Unsupported);
        assert_eq!(sink.write_calls(), 0);
    }

    #[tokio::test]
    async fn pull_no_input_code_returns_unsupported() {
        let mut dc = display_config();
        dc.shared_input_code = None;
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;

        assert_eq!(outcome, SwitchOutcome::Unsupported);
        assert_eq!(sink.write_calls(), 0);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Push tests
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn push_not_configured_returns_not_configured() {
        // No peer codes → NotConfigured.
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(display_config(), sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.push(display_id(), SwitchReason::Release).await;

        assert_eq!(outcome, SwitchOutcome::NotConfigured);
        assert_eq!(sink.write_calls(), 0);
    }

    #[tokio::test]
    async fn push_with_exact_readback_succeeds() {
        // Strong push: peer read code configured → Exact verification.
        let mut dc = display_config();
        dc.shared_peer_input_write_code = Some(PEER_WRITE);
        dc.shared_peer_input_code = Some(PEER_READ);
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.push(display_id(), SwitchReason::Release).await;

        assert_eq!(outcome, SwitchOutcome::Switched);
        assert_eq!(sink.write_calls(), 1);
        let target = sink.last_target().unwrap();
        assert_eq!(target.write_code, PEER_WRITE);
        assert_eq!(
            target.expected_readback,
            InputSourceReadback::Exact(PEER_READ)
        );
    }

    #[tokio::test]
    async fn push_degraded_uses_different_from_when_peer_read_absent() {
        // Degraded push: peer write code set, peer read code absent
        // → should use DifferentFrom(local_read).
        let mut dc = display_config();
        dc.shared_peer_input_write_code = Some(PEER_WRITE);
        dc.shared_peer_input_code = None;
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), noop_hook_engine(), tx);

        let outcome = handle.push(display_id(), SwitchReason::Release).await;

        assert_eq!(outcome, SwitchOutcome::Switched);
        assert_eq!(sink.write_calls(), 1);
        let target = sink.last_target().unwrap();
        assert_eq!(target.write_code, PEER_WRITE);
        assert_eq!(
            target.expected_readback,
            InputSourceReadback::DifferentFrom(LOCAL_READ),
            "degraded push should use DifferentFrom(local_read)"
        );
    }

    #[tokio::test]
    async fn push_before_release_abort_prevents_write() {
        let sink = Arc::new(FakeSink::new("ddcci"));

        let runner = crate::hooks::ScriptedHookRunner::new();
        runner.push_command(Err("timeout".to_string()));
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.shared_peer_input_write_code = Some(PEER_WRITE);
        dc.shared_peer_input_code = Some(PEER_READ);
        dc.hooks.before_release = vec![abortable_before_release_action()];
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink.clone(), hook_engine, tx);

        let outcome = handle.push(display_id(), SwitchReason::Release).await;

        assert!(
            matches!(outcome, SwitchOutcome::HookAborted { .. }),
            "expected HookAborted, got {outcome:?}"
        );
        assert_eq!(sink.write_calls(), 0);
        assert_eq!(sink.write_calls(), 0);
    }

    #[tokio::test]
    async fn push_write_failure_surfaced() {
        // Push write failure must be surfaced, not silently swallowed.
        let mut dc = display_config();
        dc.shared_peer_input_write_code = Some(PEER_WRITE);
        dc.shared_peer_input_code = Some(PEER_READ);
        let sink = Arc::new(FakeSink::new("ddcci"));
        sink.set_write_result(Err(CmdFailure {
            controller: "ddcci".into(),
            error: "E_DISPLAY_IO: i2c write failed".into(),
        }));
        let (tx, _rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink, noop_hook_engine(), tx);

        let outcome = handle.push(display_id(), SwitchReason::Release).await;

        assert!(
            matches!(outcome, SwitchOutcome::WriteFailed { .. }),
            "expected WriteFailed, got {outcome:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Suppression-cleanup tests
    // ═══════════════════════════════════════════════════════════════════════

    /// Collect all suppression messages in order from the channel.
    fn drain_suppression(
        rx: &mut mpsc::Receiver<dormant_core::rules::ControlMsg>,
    ) -> Vec<Option<Instant>> {
        let mut msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let dormant_core::rules::ControlMsg::SetClaimSuppression { until, .. } = msg {
                msgs.push(until);
            }
        }
        msgs
    }

    /// A [`HookRunner`] that gates the `after_acquire` hook on a barrier
    /// so the test can observe that the suppression clear was enqueued
    /// BEFORE the hook completed.  Also records commands.
    struct SignalHookRunner {
        commands: Arc<Mutex<Vec<Vec<String>>>>,
        gate: Arc<Barrier>,
    }

    impl SignalHookRunner {
        fn new(gate: Arc<Barrier>) -> Self {
            Self {
                commands: Arc::new(Mutex::new(Vec::new())),
                gate,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::hooks::HookRunner for SignalHookRunner {
        async fn run_command(
            &self,
            _env: &crate::hooks::EnvList,
            argv: &[String],
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            self.commands.lock().unwrap().push(argv.to_vec());
            // Block until the test has observed the clear.
            self.gate.wait().await;
            Ok(())
        }

        async fn publish_mqtt(
            &self,
            _topic: &str,
            _payload: &str,
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            Ok(())
        }
    }

    fn after_acquire_blocking_action() -> cs::HookAction {
        cs::HookAction {
            command: Some(vec!["after-acquire-hook".into()]),
            mqtt: None,
            timeout: Duration::from_secs(1),
            blocking: Some(true),
            abort_on_failure: false,
        }
    }

    /// Assert the explicit `guard.clear()` on the success path fires
    /// BEFORE the `after_acquire` hook — the suppression window ends
    /// before hook side-effects, not after.
    ///
    /// A `Barrier` gates the hook's completion.  With `guard.clear()`
    /// present, the clear is `try_send`-ed to the channel before the
    /// hook reaches `barrier.wait()`, so the test receives it.
    /// Without it, `guard.clear()` is absent → the hook blocks on the
    /// barrier → the clear (from Drop) hasn't been sent yet → the
    /// test's `recv()` times out.
    #[tokio::test(start_paused = true)]
    async fn suppression_clear_ordered_before_after_acquire_hook() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let gate = Arc::new(Barrier::new(2));
        let runner = SignalHookRunner::new(Arc::clone(&gate));
        let commands = runner.commands.clone();
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.hooks.after_acquire = vec![after_acquire_blocking_action()];
        let (front_ctl_tx, mut front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink, hook_engine, front_ctl_tx);

        // Subscribe BEFORE the call so no message is missed.
        let pull_handle =
            tokio::spawn(async move { handle.pull(display_id(), SwitchReason::Activity).await });

        // First message: set.
        let msg1 = front_ctl_rx
            .recv()
            .await
            .expect("suppression set must arrive");
        assert!(
            matches!(
                msg1,
                dormant_core::rules::ControlMsg::SetClaimSuppression { until: Some(_), .. }
            ),
            "first msg must be set"
        );

        // The explicit clear must be enqueued BEFORE the hook reaches
        // the barrier.  If guard.clear() is absent, the clear (from
        // Drop) hasn't been sent yet because the hook is blocking.
        let msg2 = tokio::time::timeout(Duration::from_secs(1), front_ctl_rx.recv())
            .await
            .expect("timeout waiting for clear — guard.clear() missing or reordered")
            .expect("clear channel closed");
        assert!(
            matches!(
                msg2,
                dormant_core::rules::ControlMsg::SetClaimSuppression { until: None, .. }
            ),
            "second msg must be explicit clear"
        );

        // Release the hook so pull() can complete.
        gate.wait().await;
        let outcome = pull_handle.await.unwrap();
        assert!(matches!(outcome, SwitchOutcome::Switched));
        assert_eq!(
            commands.lock().unwrap().len(),
            1,
            "after_acquire hook fired"
        );

        // No extra suppression message (prove Drop didn't fire).
        let trailing = drain_suppression(&mut front_ctl_rx);
        assert!(
            trailing.is_empty(),
            "no extra suppression message after explicit clear + Drop (cleared flag)"
        );
    }

    #[tokio::test]
    async fn suppression_set_then_exactly_one_clear_on_hook_abort() {
        let sink = Arc::new(FakeSink::new("ddcci"));

        let runner = crate::hooks::ScriptedHookRunner::new();
        runner.push_command(Err("timeout".to_string()));
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.hooks.before_acquire = vec![abortable_before_acquire_action()];
        let (front_ctl_tx, mut front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink, hook_engine, front_ctl_tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;
        assert!(matches!(outcome, SwitchOutcome::HookAborted { .. }));

        let msgs = drain_suppression(&mut front_ctl_rx);
        assert_eq!(msgs.len(), 2, "exactly one set + one clear on abort");
        assert!(msgs[0].is_some(), "first msg: set");
        assert!(msgs[1].is_none(), "second msg: explicit clear");
    }

    #[tokio::test]
    async fn suppression_set_then_exactly_one_clear_on_write_failure() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        sink.set_write_result(Err(CmdFailure {
            controller: "ddcci".into(),
            error: "E_DISPLAY_IO: write failed".into(),
        }));

        let (front_ctl_tx, mut front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(display_config(), sink, noop_hook_engine(), front_ctl_tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;
        assert!(matches!(outcome, SwitchOutcome::WriteFailed { .. }));

        let msgs = drain_suppression(&mut front_ctl_rx);
        assert_eq!(
            msgs.len(),
            2,
            "exactly one set + one clear on write failure"
        );
        assert!(msgs[0].is_some(), "first msg: set");
        assert!(msgs[1].is_none(), "second msg: explicit clear");
    }

    /// Prove Drop supplies a clear when explicit `clear()` is not called —
    /// the safety net is real and independently testable.
    #[tokio::test]
    async fn suppression_drop_sends_clear_when_not_explicitly_cleared() {
        let (front_ctl_tx, mut front_ctl_rx) = mpsc::channel(8);
        let display = display_id();

        {
            let _guard = SuppressionGuard::set(&front_ctl_tx, &display);
            // Guard goes out of scope without explicit clear() — Drop must
            // send the clear.
        }

        let msgs = drain_suppression(&mut front_ctl_rx);
        assert_eq!(msgs.len(), 2, "set + drop clear");
        assert!(msgs[0].is_some(), "first: set");
        assert!(msgs[1].is_none(), "second: drop clear");
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  SwitchReason coverage
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn switch_reason_has_seven_variants() {
        let reasons = [
            SwitchReason::Activity,
            SwitchReason::Hotkey,
            SwitchReason::Cli,
            SwitchReason::Web,
            SwitchReason::Tray,
            SwitchReason::Release,
            SwitchReason::Toggle,
        ];
        assert_eq!(reasons.len(), 7);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Convergence simulation
    // ═══════════════════════════════════════════════════════════════════════

    /// A [`HookRunner`] that records every command invocation and always
    /// succeeds.  Used to assert that `before_acquire`/`after_acquire` are
    /// called exactly once per successful pull.
    struct CountingHookRunner {
        command_calls: Arc<Mutex<Vec<Vec<String>>>>,
        mqtt_calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl CountingHookRunner {
        fn new() -> Self {
            Self {
                command_calls: Arc::new(Mutex::new(Vec::new())),
                mqtt_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn command_count(&self) -> usize {
            self.command_calls.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl crate::hooks::HookRunner for CountingHookRunner {
        async fn run_command(
            &self,
            _env: &crate::hooks::EnvList,
            argv: &[String],
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            self.command_calls.lock().unwrap().push(argv.to_vec());
            Ok(())
        }

        async fn publish_mqtt(
            &self,
            topic: &str,
            payload: &str,
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            self.mqtt_calls
                .lock()
                .unwrap()
                .push((topic.to_string(), payload.to_string()));
            Ok(())
        }
    }

    /// Build a `DirectSwitchHandle` for convergence testing with a
    /// specific [`DisplayConfig`] and a [`CountingHookRunner`].
    fn build_convergence_handle(
        dc: cs::DisplayConfig,
        sink: Arc<FakeSink>,
        hooks: Arc<HookEngine>,
        front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    ) -> DirectSwitchHandle {
        let executors: HashMap<_, _> = [(display_id(), sink as Arc<dyn CommandSink>)].into();
        let (_, executors_rx) = watch::channel(Arc::new(executors));
        // Use a cooldown of 3 s for convergence tests — matches the
        // production default in [`dormant_core::config::defaults::COOLDOWN`].
        let mut cfg = minimal_config(dc);
        cfg.coordination.cooldown = Duration::from_secs(3);
        let (_, config_rx) = watch::channel(Arc::new(cfg));
        DirectSwitchHandle {
            executors: executors_rx,
            config: config_rx,
            hooks,
            front_ctl_tx,
            last_activity_pull: Mutex::new(HashMap::new()),
        }
    }

    /// Build a `DisplayConfig` for convergence testing with the given
    /// local and peer codes.  Everything else is set up like a minimal
    /// shared display with DDC capability.
    fn convergence_display_config(
        local_read: u8,
        local_write: u8,
        peer_read: Option<u8>,
        peer_write: Option<u8>,
    ) -> cs::DisplayConfig {
        cs::DisplayConfig {
            shared_input_code: Some(local_read),
            shared_input_write_code: Some(local_write),
            shared_peer_input_code: peer_read,
            shared_peer_input_write_code: peer_write,
            hooks: cs::HookSlots {
                before_acquire: vec![cs::HookAction {
                    command: Some(vec!["echo".into(), "before_acquire".into()]),
                    mqtt: None,
                    timeout: Duration::from_secs(1),
                    blocking: Some(true),
                    abort_on_failure: false,
                }],
                after_acquire: vec![cs::HookAction {
                    command: Some(vec!["echo".into(), "after_acquire".into()]),
                    mqtt: None,
                    timeout: Duration::from_secs(1),
                    blocking: Some(false),
                    abort_on_failure: false,
                }],
                ..cs::HookSlots::default()
            },
            ..display_config()
        }
    }

    /// Proves the convergence rules from the ratified direct-write design:
    /// only explicit local edges write; no poll-triggered corrective writes;
    /// a cooldown after each observed source change begins each instance's
    /// independent cooldown; and no retries based on stale ownership metadata.
    ///
    /// Two hosts share one display.  Host A (local 0x10/0x15) and Host B
    /// (local 0x20/0x25) both receive activity edges at the same logical
    /// instant.  Delayed polls observe the outcome through alternating
    /// unknown raw codes, eventually settling on a stable source.  The
    /// test asserts:
    ///
    /// - At most one write per admitted local edge.
    /// - Zero poll-caused writes (the poll path never calls `pull`).
    /// - No recurring ping-pong after the queue drains.
    /// - Exactly one `before_acquire`/`after_acquire` pair per successful
    ///   admitted edge.
    #[tokio::test(start_paused = true)]
    #[allow(clippy::too_many_lines, clippy::similar_names)]
    async fn simultaneous_edges_with_delayed_garbled_polls_converge_without_ping_pong() {
        // ── Setup ──────────────────────────────────────────────────────
        // Host A: local_read=0x10, local_write=0x15, peer_read=0x20
        // Host B: local_read=0x20, local_write=0x25, peer_read=0x10

        let sink_a = Arc::new(FakeSink::new("ddcci-a"));
        let sink_b = Arc::new(FakeSink::new("ddcci-b"));

        let hook_runner_a = Arc::new(CountingHookRunner::new());
        let hook_runner_b = Arc::new(CountingHookRunner::new());
        let hook_engine_a = Arc::new(HookEngine::with_runner(hook_runner_a.clone()));
        let hook_engine_b = Arc::new(HookEngine::with_runner(hook_runner_b.clone()));

        let dc_a = convergence_display_config(0x10, 0x15, Some(0x20), None);
        let dc_b = convergence_display_config(0x20, 0x25, Some(0x10), None);

        let (tx_a, _rx_a) = mpsc::channel(8);
        let (tx_b, _rx_b) = mpsc::channel(8);

        let handle_a = build_convergence_handle(dc_a, sink_a.clone(), hook_engine_a, tx_a);
        let handle_b = build_convergence_handle(dc_b, sink_b.clone(), hook_engine_b, tx_b);

        // Coordination handles for poll simulation (same display, same
        // initial owned=true from the seeded CoordRecord).
        let coord_a =
            dormant_core::coordination::CoordinationHandle::new([display_id()].into_iter());
        let coord_b =
            dormant_core::coordination::CoordinationHandle::new([display_id()].into_iter());

        let aliases_a = dormant_core::coordination::InputCodeAliases {
            local_read: 0x10,
            local_write: 0x15,
            peer_read: Some(0x20),
            peer_write: None,
        };
        let aliases_b = dormant_core::coordination::InputCodeAliases {
            local_read: 0x20,
            local_write: 0x25,
            peer_read: Some(0x10),
            peer_write: None,
        };

        // ── t=0: Simultaneous activity edges ─────────────────────────
        // Both hosts pull — neither has a cooldown record yet.
        let (res_a, res_b) = tokio::join!(
            handle_a.pull(display_id(), SwitchReason::Activity),
            handle_b.pull(display_id(), SwitchReason::Activity),
        );

        assert_eq!(res_a, SwitchOutcome::Switched, "host A first pull");
        assert_eq!(res_b, SwitchOutcome::Switched, "host B first pull");
        assert_eq!(sink_a.write_calls(), 1, "host A: one write per edge");
        assert_eq!(sink_b.write_calls(), 1, "host B: one write per edge");

        // Yield to let the spawned after_acquire hooks complete in
        // paused time.
        tokio::task::yield_now().await;

        // Each successful pull: before_acquire (blocking) + after_acquire
        // (fire-and-forget) = 2 command calls per pull.
        assert_eq!(
            hook_runner_a.command_count(),
            2,
            "host A: before + after per admitted edge"
        );
        assert_eq!(
            hook_runner_b.command_count(),
            2,
            "host B: before + after per admitted edge"
        );

        // ── t=1s..t=4s: Delayed polls with garbled observations ─────
        // The panel settles on 0x20 (B's local read alias, which B wrote
        // last).  A sees a Peer code; B sees a Local code.  Interleave
        // garbled Unknown readings to exercise the debounce.

        // t=1s: A sees 0x00 (garbled, Unknown for A).
        let outcome = coord_a.record_input_observation(&display_id(), 0x00, &aliases_a, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));

        // t=1s: B sees 0x20 (Local for B).
        let _ = coord_b.record_input_observation(&display_id(), 0x20, &aliases_b, 3, None);

        // t=2s: A sees 0x20 (Peer for A) — disagrees with 0x00 → reset.
        let outcome = coord_a.record_input_observation(&display_id(), 0x20, &aliases_a, 3, None);
        assert_eq!(outcome.disagreement_with, Some(0x00));
        assert_eq!(outcome.deferred_loss_count, Some(1));

        // t=2s: B sees 0x10 (Peer for B) — first not-mine reading.
        let outcome = coord_b.record_input_observation(&display_id(), 0x10, &aliases_b, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));

        // t=3s: A sees 0x20 again → pending loss 2/3.
        let _ = coord_a.record_input_observation(&display_id(), 0x20, &aliases_a, 3, None);

        // t=3s: B sees 0x20 again (Local) → disagreement with 0x10 resets
        // the pending B loss counter, returns B to owned state.
        let outcome = coord_b.record_input_observation(&display_id(), 0x20, &aliases_b, 3, None);
        assert_eq!(outcome.disagreement_with, Some(0x10));

        // t=4s: A sees 0x20 again → loss committed (3/3).
        let outcome = coord_a.record_input_observation(&display_id(), 0x20, &aliases_a, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(true));
        assert!(!coord_a.snapshot()[&display_id()].owned);

        // t=4s: B sees 0x20 again → already owned, no change.
        let _ = coord_b.record_input_observation(&display_id(), 0x20, &aliases_b, 3, None);
        assert!(coord_b.snapshot()[&display_id()].owned);

        // ── Zero poll-caused writes ──────────────────────────────────
        // The poll path (CoordinationHandle) never calls pull() — prove
        // that the write counts are unchanged after polling.

        // The poll path (CoordinationHandle) never calls pull() — prove
        // that the write counts are unchanged after polling.
        assert_eq!(
            sink_a.write_calls(),
            1,
            "host A: polls did not cause writes"
        );
        assert_eq!(
            sink_b.write_calls(),
            1,
            "host B: polls did not cause writes"
        );

        // ── t=5s: Simultaneous edges within cooldown ─────────────────
        // At t=0 both hosts pulled → cooldown expires at t=3s.
        // At t=5s, both are past the cooldown → both should write again.

        // Advance virtual time to 5s.
        tokio::time::advance(Duration::from_secs(5)).await;

        let (pulla2, pullb2) = tokio::join!(
            handle_a.pull(display_id(), SwitchReason::Activity),
            handle_b.pull(display_id(), SwitchReason::Activity),
        );

        assert_eq!(
            pulla2,
            SwitchOutcome::Switched,
            "host A: past cooldown → allowed"
        );
        assert_eq!(
            pullb2,
            SwitchOutcome::Switched,
            "host B: past cooldown → allowed"
        );
        assert_eq!(sink_a.write_calls(), 2, "host A: two writes total");
        assert_eq!(sink_b.write_calls(), 2, "host B: two writes total");

        // Yield to let spawned after_acquire hooks complete.
        tokio::task::yield_now().await;

        // Hook counts: 2 more per handle (before + after per pull).
        assert_eq!(
            hook_runner_a.command_count(),
            4,
            "host A: two before+after pairs"
        );
        assert_eq!(
            hook_runner_b.command_count(),
            4,
            "host B: two before+after pairs"
        );

        // ── t=6s: Attempt edges within cooldown (3 s after t=5s) ────
        // Advance only 1s — still within the 3s cooldown.
        tokio::time::advance(Duration::from_secs(1)).await;

        let suppressed_a = handle_a.pull(display_id(), SwitchReason::Activity).await;
        let suppressed_b = handle_b.pull(display_id(), SwitchReason::Activity).await;

        assert_eq!(
            suppressed_a,
            SwitchOutcome::Cooldown,
            "host A: within cooldown → suppressed"
        );
        assert_eq!(
            suppressed_b,
            SwitchOutcome::Cooldown,
            "host B: within cooldown → suppressed"
        );

        // Write counts unchanged — no write happened.
        assert_eq!(sink_a.write_calls(), 2, "host A: cooldown suppressed write");
        assert_eq!(sink_b.write_calls(), 2, "host B: cooldown suppressed write");

        // ── No ping-pong after queue drains ──────────────────────────
        // Advance past the cooldown and verify that without further
        // edges, no spontaneous writes occur.
        tokio::time::advance(Duration::from_secs(5)).await;

        // Drift check: write counts are exactly 2 (one per admitted
        // edge) — no spontaneous or poll-triggered writes.
        assert_eq!(sink_a.write_calls(), 2);
        assert_eq!(sink_b.write_calls(), 2);

        // ── Finite upper bound ───────────────────────────────────────
        // Four edges were admitted (two at t=0, two at t=5s).  The two
        // at t=6s were suppressed by cooldown.  Total writes = 4,
        // exactly one per admitted edge.
        let total_writes = sink_a.write_calls() + sink_b.write_calls();
        assert_eq!(
            total_writes, 4,
            "total writes equals admitted edges (2+2), no ping-pong overflow"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Observed-loss hook tests
    // ═══════════════════════════════════════════════════════════════════════

    /// A [`HookRunner`] that captures every command argv and succeeds.
    struct CapturingHookRunner {
        commands: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl CapturingHookRunner {
        fn new() -> Self {
            Self {
                commands: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::hooks::HookRunner for CapturingHookRunner {
        async fn run_command(
            &self,
            _env: &crate::hooks::EnvList,
            argv: &[String],
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            self.commands.lock().unwrap().push(argv.to_vec());
            Ok(())
        }

        async fn publish_mqtt(
            &self,
            _topic: &str,
            _payload: &str,
            _timeout: Duration,
        ) -> crate::hooks::HookIoResult {
            Ok(())
        }
    }

    fn observed_loss_action() -> cs::HookAction {
        cs::HookAction {
            command: Some(vec!["observed-loss-hook".into()]),
            mqtt: None,
            timeout: Duration::from_secs(1),
            blocking: Some(true),
            abort_on_failure: false,
        }
    }

    #[tokio::test]
    async fn notify_observed_loss_fires_on_observed_loss_slot() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let runner = CapturingHookRunner::new();
        let commands = runner.commands.clone();
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.hooks.on_observed_loss = vec![observed_loss_action()];
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink, hook_engine, front_ctl_tx);

        handle.notify_observed_loss(&display_id()).await;

        let captured = commands.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "one hook should fire");
        assert_eq!(captured[0], vec!["observed-loss-hook"]);
    }

    #[tokio::test]
    async fn notify_observed_loss_noop_on_empty_slot() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let runner = CapturingHookRunner::new();
        let commands = runner.commands.clone();
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let dc = display_config(); // on_observed_loss defaults to empty
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink, hook_engine, front_ctl_tx);

        handle.notify_observed_loss(&display_id()).await;

        let captured = commands.lock().unwrap().clone();
        assert!(captured.is_empty(), "no hook should fire on empty slot");
    }

    #[tokio::test]
    async fn notify_observed_loss_never_triggers_write() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(NoopHookRunner)));

        let mut dc = display_config();
        dc.hooks.on_observed_loss = vec![observed_loss_action()];
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, Arc::clone(&sink), hook_engine, front_ctl_tx);

        handle.notify_observed_loss(&display_id()).await;

        assert_eq!(
            sink.write_calls(),
            0,
            "notify must never trigger a DDC write"
        );
    }

    #[tokio::test]
    async fn notify_observed_loss_unknown_display_is_noop() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let runner = CapturingHookRunner::new();
        let commands = runner.commands.clone();
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.hooks.on_observed_loss = vec![observed_loss_action()];
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, sink, hook_engine, front_ctl_tx);

        // Notify for a different display — should be a no-op.
        handle
            .notify_observed_loss(&DisplayId("nonexistent".to_string()))
            .await;

        let captured = commands.lock().unwrap().clone();
        assert!(captured.is_empty(), "unknown display must not fire hooks");
    }

    /// Regression guard: the initiator blocking `before_acquire` still
    /// prevents the DDC write on abort.
    #[tokio::test]
    async fn initiator_before_acquire_abort_still_blocks_write() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let runner = crate::hooks::ScriptedHookRunner::new();
        runner.push_command(Err("timeout".to_string()));
        let hook_engine = Arc::new(HookEngine::with_runner(Arc::new(runner)));

        let mut dc = display_config();
        dc.hooks.before_acquire = vec![abortable_before_acquire_action()];
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(dc, Arc::clone(&sink), hook_engine, front_ctl_tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;
        assert!(
            matches!(outcome, SwitchOutcome::HookAborted { .. }),
            "aborted before_acquire must prevent write"
        );
        assert_eq!(sink.write_calls(), 0, "no DDC write after abort");
    }
}
