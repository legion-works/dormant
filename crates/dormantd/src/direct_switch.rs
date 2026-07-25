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
        }
    }

    /// Pull the display to this machine — write the local input code.
    ///
    /// Always available on a shared display with a configured
    /// `shared_input_code`.  The path validates the display, runs
    /// `before_acquire` hooks (blocking), writes the input-source
    /// command, clears claim suppression on every exit, and runs
    /// `after_acquire` only on success.
    pub async fn pull(&self, display: DisplayId, _reason: SwitchReason) -> SwitchOutcome {
        let Some((dc, target)) = self.resolve_local_target(&display) else {
            return SwitchOutcome::Unsupported;
        };

        let Some(executor) = self.resolve_executor(&display) else {
            return SwitchOutcome::Unsupported;
        };

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

    #[tokio::test]
    async fn suppression_set_and_cleared_on_success() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        let (front_ctl_tx, mut front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(display_config(), sink, noop_hook_engine(), front_ctl_tx);

        let _ = handle.pull(display_id(), SwitchReason::Activity).await;

        // Drain channel: first msg = SetClaimSuppression with Some(until),
        // second msg = SetClaimSuppression with None (clear).
        let mut saw_set = false;
        let mut saw_clear = false;
        while let Ok(msg) = front_ctl_rx.try_recv() {
            if let dormant_core::rules::ControlMsg::SetClaimSuppression { until, .. } = msg {
                if until.is_some() {
                    saw_set = true;
                } else {
                    saw_clear = true;
                }
            }
        }

        assert!(saw_set, "suppression set message must be sent");
        assert!(saw_clear, "suppression clear message must be sent");
    }

    #[tokio::test]
    async fn suppression_cleared_on_hook_abort() {
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

        // Must see a clear message.
        let mut saw_clear = false;
        while let Ok(msg) = front_ctl_rx.try_recv() {
            if let dormant_core::rules::ControlMsg::SetClaimSuppression { until: None, .. } = msg {
                saw_clear = true;
            }
        }
        assert!(saw_clear, "suppression must be cleared on hook abort");
    }

    #[tokio::test]
    async fn suppression_cleared_on_write_failure() {
        let sink = Arc::new(FakeSink::new("ddcci"));
        sink.set_write_result(Err(CmdFailure {
            controller: "ddcci".into(),
            error: "E_DISPLAY_IO: write failed".into(),
        }));

        let (front_ctl_tx, mut front_ctl_rx) = mpsc::channel(8);
        let handle = build_handle(display_config(), sink, noop_hook_engine(), front_ctl_tx);

        let outcome = handle.pull(display_id(), SwitchReason::Activity).await;
        assert!(matches!(outcome, SwitchOutcome::WriteFailed { .. }));

        let mut saw_clear = false;
        while let Ok(msg) = front_ctl_rx.try_recv() {
            if let dormant_core::rules::ControlMsg::SetClaimSuppression { until: None, .. } = msg {
                saw_clear = true;
            }
        }
        assert!(saw_clear, "suppression must be cleared on write failure");
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
}
