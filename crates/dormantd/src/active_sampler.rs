//! Pure lifecycle rules and capture boundary for active wear sampling.

use async_trait::async_trait;
use dormant_core::config::schema::{ActiveSamplingConfig, Config, StreamMode};
use dormant_core::ipc_proto::WearSamplingStatus;
use dormant_core::rules::{ControlMsg, DaemonEvent};
use dormant_core::spatial_grid::LumaGrid;
use dormant_core::state_machine::Phase;
use dormant_core::types::Tick;
use dormant_core::wear::{WearSamplingState, WearSamplingStatus as RedactedWearSamplingStatus};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
pub mod linux;

/// Stable fallback reason when sampling needs a new portal grant.
pub const WEAR_SAMPLING_NEEDS_CONSENT: &str = "wear_sampling_needs_consent";
/// Stable fallback reason for a timed-out or rejected consent flow.
pub const WEAR_SAMPLING_CONSENT_TIMEOUT: &str = "wear_sampling_consent_timeout";
/// Stable fallback reason when the portal transport cannot be reached.
pub const WEAR_SAMPLING_PORTAL_UNREACHABLE: &str = "wear_sampling_portal_unreachable";
/// Stable fallback reason for an invalid or revoked portal token.
pub const WEAR_SAMPLING_TOKEN_INVALID: &str = "wear_sampling_token_invalid";
/// Stable fallback reason when a consent binding targets a changed display.
pub const WEAR_SAMPLING_DISPLAY_CHANGED: &str = "wear_sampling_display_changed";
/// Stable fallback reason when a granted stream does not match its display.
pub const WEAR_SAMPLING_WRONG_MONITOR: &str = "wear_sampling_wrong_monitor";
const CONSENT_INTERACTION_TIMEOUT: Duration = Duration::from_secs(300);
/// Stable fallback reason for a capture failure before the breaker opens.
pub const WEAR_SAMPLING_CAPTURE_FAILED: &str = "wear_sampling_capture_failed";
/// Stable fallback reason while the capture circuit breaker is open.
pub const WEAR_SAMPLING_COOLDOWN: &str = "wear_sampling_cooldown";
/// Stable fallback reason while administrative settings suspend sampling.
pub const WEAR_SAMPLING_SUSPENDED: &str = "wear_sampling_suspended";

/// Privacy-preserving frame sample made available to the wear tracker.
#[derive(Debug, Clone, PartialEq)]
pub struct SampledGrid {
    /// Reduced luma values; the raw portal frame has already been discarded.
    pub grid: LumaGrid,
    /// Monotonic instant at which the capture completed.
    pub captured_at: Tick,
    /// Display phase observed when the capture was requested.
    pub phase_at_capture: Phase,
}

/// Most recent privacy-preserving sample shared with the wear tracker.
pub type LatestGrid = Arc<RwLock<Option<SampledGrid>>>;

/// Public lifecycle snapshot for future IPC and status consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplerStatus {
    /// Current sampling lifecycle state.
    pub state: SamplingState,
    /// Monotonic time of the most recent successful sample.
    pub last_capture: Option<Tick>,
    /// Stable uniform-attribution reason while not streaming.
    pub uniform_reason: Option<&'static str>,
    /// Configured display bound to the current consent record.
    pub bound_display: Option<String>,
    /// Grant wall-clock timestamp, exposed without any portal identifiers.
    pub granted_at: Option<OffsetDateTime>,
}

impl SamplerStatus {
    /// Convert daemon-local monotonic state into the portable redacted wire view.
    #[must_use]
    pub fn redacted(&self, now: Tick) -> RedactedWearSamplingStatus {
        let state = match self.state {
            SamplingState::Disabled => WearSamplingState::Disabled,
            SamplingState::NeedsConsent => WearSamplingState::NeedsConsent,
            SamplingState::ConsentPending => WearSamplingState::ConsentPending,
            SamplingState::Connecting => WearSamplingState::Connecting,
            SamplingState::Streaming => WearSamplingState::Streaming,
            SamplingState::Suspended => WearSamplingState::Suspended,
            SamplingState::Cooldown => WearSamplingState::Cooldown,
        };
        RedactedWearSamplingStatus {
            state,
            last_capture_age_s: self
                .last_capture
                .map(|capture| now.0.saturating_duration_since(capture.0).as_secs()),
            uniform_reason: self.uniform_reason.map(str::to_owned),
            bound_display: self.bound_display.clone(),
            granted_at_epoch_s: self.granted_at.map(OffsetDateTime::unix_timestamp),
        }
    }
}

/// Sender and status subscription for the daemon's single sampler service.
#[derive(Clone)]
pub struct ActiveSamplerHandle {
    command_tx: mpsc::Sender<SamplerCommand>,
    status_rx: watch::Receiver<SamplerStatus>,
}

/// Result reported by an explicit consent command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentFlowStatus {
    /// The portal consent UI is open.
    AwaitingConsent,
    /// Consent was granted and its validated record was stored.
    Granted,
    /// The operator denied the portal request.
    Denied,
    /// The portal request exceeded its deadline.
    TimedOut,
    /// The portal flow failed with a stable reason.
    Error(String),
}

impl ConsentFlowStatus {
    pub(crate) fn into_ipc_status(self) -> WearSamplingStatus {
        match self {
            Self::AwaitingConsent => {
                unreachable!("consent flow replies only after terminal status")
            }
            Self::Granted => WearSamplingStatus::Granted,
            Self::Denied => WearSamplingStatus::Denied,
            Self::TimedOut => WearSamplingStatus::TimedOut,
            Self::Error(reason) => WearSamplingStatus::Error(reason),
        }
    }
}

/// Error returned while routing a sampler command.
#[derive(Debug)]
pub enum SamplerError {
    /// Active sampling is disabled in configuration.
    DisabledByConfig,
    /// Another consent request is already active.
    FlowAlreadyActive,
    /// The portal source could not be created for this graphical session.
    NoGraphicalSession,
    /// The daemon sampler is no longer running.
    CommandChannelClosed,
    /// Persistent consent-record I/O failed.
    Store(crate::screencast_consent::ConsentError),
}

/// Requests accepted by the daemon-lifetime sampler.
pub enum SamplerCommand {
    /// Open the explicit portal consent flow.
    Enable {
        /// Receives the resulting flow status.
        reply: oneshot::Sender<ConsentFlowStatus>,
    },
    /// Disable sampling, optionally forgetting the stored record.
    Disable {
        /// Remove the consent record as well as closing the session.
        forget: bool,
        /// Receives the command outcome.
        reply: oneshot::Sender<Result<(), SamplerError>>,
    },
}

/// Inputs delivered to the service without replacing its daemon lifetime.
pub enum SamplerUpdate {
    /// Relevant active-sampling configuration change.
    Reconfigure(ReconfigurePlan),
    /// Current sampled-display phase from the active generation.
    DisplayContext(DisplaySamplingContext),
}

/// Runtime configuration and its lifecycle trigger, constructed by Task 9.
#[derive(Debug, Clone)]
pub struct ReconfigurePlan {
    /// New active-sampling settings.
    pub active_sampling: ActiveSamplingConfig,
    /// Shared cadence with the wear tracker.
    pub sample_interval: Duration,
    /// Lifecycle transition implied by the configuration diff.
    pub trigger: ConfigDelta,
}

/// Dependencies owned by the active sampler shell.
pub struct ActiveSamplerDeps {
    /// Initial daemon configuration used before reload updates arrive.
    pub initial_config: Arc<Config>,
    /// Reload and generation-context updates.
    pub update_rx: mpsc::Receiver<SamplerUpdate>,
    /// Daemon-lifetime latest-value handoff to the wear tracker.
    pub latest_grid: LatestGrid,
    /// Platform capture implementation.
    pub source: Box<dyn CaptureSource + Send + Sync + 'static>,
    /// Secure persisted portal-consent record path.
    pub consent_path: PathBuf,
    /// Daemon shutdown signal.
    pub cancel: CancellationToken,
    /// Environment lookup used to verify that a graphical session exists.
    pub env_reader: fn(&str) -> Option<String>,
    /// Front control channel used for sampler lifecycle events.
    pub event_tx: Option<mpsc::Sender<ControlMsg>>,
}

pub(crate) fn production_env_reader(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Display identity and active phase supplied by generation management.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplaySamplingContext {
    /// Configured sampled display, when present in the generation.
    pub display: Option<DisplayExpectation>,
    /// Current display phase.
    pub phase: Phase,
    /// Whether a display stage currently permits spatial attribution.
    pub stage_active: bool,
}

/// Allocate the daemon-lifetime latest-sample slot.
#[must_use]
pub fn new_latest_grid() -> LatestGrid {
    Arc::new(RwLock::new(None))
}

fn replace_latest(latest: &LatestGrid, sample: SampledGrid) {
    if let Ok(mut slot) = latest.write() {
        *slot = Some(sample);
    }
}

impl ActiveSamplerHandle {
    fn new(
        status: SamplerStatus,
    ) -> (
        Self,
        mpsc::Receiver<SamplerCommand>,
        watch::Sender<SamplerStatus>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (status_tx, status_rx) = watch::channel(status);
        (
            Self {
                command_tx,
                status_rx,
            },
            command_rx,
            status_tx,
        )
    }

    /// Subscribe to status changes without exposing consent secrets.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<SamplerStatus> {
        self.status_rx.clone()
    }

    /// Send a command to the daemon sampler.
    ///
    /// # Errors
    ///
    /// Returns [`SamplerError::CommandChannelClosed`] after daemon shutdown.
    pub async fn send(&self, command: SamplerCommand) -> Result<(), SamplerError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| SamplerError::CommandChannelClosed)
    }
}

impl fmt::Display for SamplerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisabledByConfig => f.write_str("active sampling is disabled by configuration"),
            Self::FlowAlreadyActive => {
                f.write_str("an active sampling consent flow is already active")
            }
            Self::NoGraphicalSession => f.write_str("no graphical session is available"),
            Self::CommandChannelClosed => f.write_str("active sampling service is not running"),
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SamplerError {}

/// Spawn the sampler with an internal command channel.
///
/// Daemon wiring that needs the command handle should use
/// [`spawn_with_handle`].
#[must_use]
pub fn spawn(deps: ActiveSamplerDeps) -> JoinHandle<()> {
    let initial = initial_status(&deps.initial_config);
    let (handle, command_rx, status_tx) = ActiveSamplerHandle::new(initial);
    tokio::spawn(async move {
        // Keeping this sender alive prevents a detached sampler from treating its
        // internal command receiver as a shutdown signal.
        let _handle = handle;
        run(deps, command_rx, status_tx).await;
    })
}

/// Spawn the sampler and return the command/status handle for daemon routing.
#[must_use]
pub fn spawn_with_handle(deps: ActiveSamplerDeps) -> (ActiveSamplerHandle, JoinHandle<()>) {
    let initial = initial_status(&deps.initial_config);
    let (handle, command_rx, status_tx) = ActiveSamplerHandle::new(initial);
    let join = tokio::spawn(run(deps, command_rx, status_tx));
    (handle, join)
}

fn initial_status(config: &Config) -> SamplerStatus {
    let enabled = config.wear.active_sampling.enabled;
    let state = if enabled {
        SamplingState::NeedsConsent
    } else {
        SamplingState::Disabled
    };
    SamplerStatus {
        state,
        last_capture: None,
        uniform_reason: enabled.then_some(WEAR_SAMPLING_NEEDS_CONSENT),
        bound_display: config.wear.active_sampling.sampled_display.clone(),
        granted_at: None,
    }
}

struct Runtime {
    state: SamplingState,
    active: ActiveSamplingConfig,
    sample_interval: Duration,
    display: DisplaySamplingContext,
    record: Option<crate::screencast_consent::BoundConsent>,
    failures: u32,
    reconnect_backoff: Duration,
    episode_warned: std::collections::HashSet<String>,
    pending_stream_reset: bool,
    event_tx: Option<mpsc::Sender<ControlMsg>>,
}

impl Runtime {
    fn new(config: &Config, consent_path: &std::path::Path) -> Self {
        let display = DisplaySamplingContext {
            display: config
                .wear
                .active_sampling
                .sampled_display
                .as_ref()
                .map(|display| DisplayExpectation {
                    display: display.clone(),
                }),
            phase: Phase::Active,
            stage_active: true,
        };
        let active = config.wear.active_sampling.clone();
        let record = display.display.as_ref().and_then(|expected| {
            crate::screencast_consent::load(consent_path, &expected.display).ok()
        });
        let state = if !active.enabled {
            SamplingState::Disabled
        } else if !config.wear.enabled || display.display.is_none() {
            SamplingState::Suspended
        } else if record.is_some() {
            SamplingState::Connecting
        } else {
            SamplingState::NeedsConsent
        };
        Self {
            state,
            active,
            sample_interval: config.wear.sample_interval,
            display,
            record,
            failures: 0,
            reconnect_backoff: Duration::from_secs(30),
            episode_warned: std::collections::HashSet::new(),
            pending_stream_reset: false,
            event_tx: None,
        }
    }

    fn display_name(&self) -> String {
        self.display
            .display
            .as_ref()
            .map_or_else(|| "unbound".to_owned(), |display| display.display.clone())
    }
}

enum ConnectOutcome {
    Cancelled,
    Connected(ConnectedStream),
    NeedsConsent(&'static str),
    Transport,
}

enum CaptureOutcome {
    Cancelled,
    Ok(Tick),
    Failed(CaptureError, u32),
}

async fn connect(
    source: &mut dyn CaptureSource,
    record: &crate::screencast_consent::BoundConsent,
    cancel: &CancellationToken,
) -> ConnectOutcome {
    let binding = record.as_binding();
    tokio::select! {
        () = cancel.cancelled() => ConnectOutcome::Cancelled,
        outcome = source.connect(&binding) => match outcome {
            Ok(stream) => ConnectOutcome::Connected(stream),
            Err(CaptureError::Auth | CaptureError::SessionClosed) => ConnectOutcome::NeedsConsent(WEAR_SAMPLING_TOKEN_INVALID),
            Err(CaptureError::Protocol(reason)) if reason == WEAR_SAMPLING_WRONG_MONITOR => ConnectOutcome::NeedsConsent(WEAR_SAMPLING_WRONG_MONITOR),
            Err(CaptureError::ConsentDenied | CaptureError::Timeout | CaptureError::Transport(_) | CaptureError::Protocol(_)) => ConnectOutcome::Transport,
        },
    }
}

async fn capture_one(
    source: &mut dyn CaptureSource,
    active: &ActiveSamplingConfig,
    phase: Phase,
    latest: &LatestGrid,
    cancel: &CancellationToken,
    cadence: &mut tokio::time::Interval,
    reset_stream: bool,
) -> CaptureOutcome {
    if reset_stream {
        source.reset_stream().await;
    }
    let capture = tokio::time::timeout(
        active.capture_timeout,
        source.capture_one(active.stream_mode),
    );
    tokio::pin!(capture);
    let mut overlapping = 0;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return CaptureOutcome::Cancelled,
            result = &mut capture => match result {
                Ok(Ok(frame)) => match dormant_core::spatial_grid::reduce_rgba8_to_luma_grid(
                    &frame.rgba, frame.width, frame.height, frame.stride, 9, 16,
                ) {
                    Ok(grid) => {
                        let captured_at = Tick::now();
                        replace_latest(latest, SampledGrid {
                            grid,
                            captured_at,
                            phase_at_capture: phase,
                        });
                        return CaptureOutcome::Ok(captured_at);
                    }
                    Err(_) => return CaptureOutcome::Failed(CaptureError::Protocol("grid reduction failed".to_owned()), overlapping),
                },
                Ok(Err(error)) => return CaptureOutcome::Failed(error, overlapping),
                Err(_) => return CaptureOutcome::Failed(CaptureError::Timeout, overlapping),
            },
            _ = cadence.tick() => {
                overlapping = overlapping.saturating_add(1);
            }
        }
    }
}

fn persist_rotated_token(
    runtime: &mut Runtime,
    stream: ConnectedStream,
    path: &std::path::Path,
) -> Result<(), crate::screencast_consent::ConsentError> {
    let Some(record) = runtime.record.as_ref() else {
        return Ok(());
    };
    let mut rotated = record.record().clone();
    rotated.token = stream.restore_token;
    crate::screencast_consent::store_atomic(path, &rotated)?;
    runtime.record = crate::screencast_consent::load(path, &rotated.sampled_display).ok();
    Ok(())
}

fn apply_capture_failure(
    runtime: &mut Runtime,
    error: CaptureError,
    status_tx: &watch::Sender<SamplerStatus>,
) -> Transition {
    match error {
        CaptureError::Auth => apply_trigger(runtime, Trigger::AuthFailed, status_tx),
        CaptureError::SessionClosed => apply_trigger(runtime, Trigger::SessionClosed, status_tx),
        CaptureError::Protocol(reason) if reason == WEAR_SAMPLING_WRONG_MONITOR => {
            apply_trigger(runtime, Trigger::WrongMonitor, status_tx)
        }
        CaptureError::Transport(_) => apply_trigger(runtime, Trigger::TransportFailed, status_tx),
        CaptureError::ConsentDenied | CaptureError::Timeout | CaptureError::Protocol(_) => {
            if runtime.failures >= runtime.active.failure_threshold {
                apply_trigger(runtime, Trigger::CaptureFailed, status_tx)
            } else {
                // Below the breaker threshold the lifecycle remains Streaming;
                // only its uniform fallback changes for this failed tick.
                publish_status(status_tx, runtime, Some(WEAR_SAMPLING_CAPTURE_FAILED), None);
                Transition {
                    next: runtime.state,
                    effects: vec![Effect::EnterUniform(WEAR_SAMPLING_CAPTURE_FAILED)],
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "the command lifecycle keeps each consent result adjacent to its persistent-record outcome"
)]
async fn handle_command(
    runtime: &mut Runtime,
    source: &mut dyn CaptureSource,
    consent_path: &std::path::Path,
    command: SamplerCommand,
    command_rx: &mut mpsc::Receiver<SamplerCommand>,
    status_tx: &watch::Sender<SamplerStatus>,
    cancel: &CancellationToken,
    env_reader: fn(&str) -> Option<String>,
) -> bool {
    match command {
        SamplerCommand::Enable { reply } => {
            if !runtime.active.enabled {
                let _ = reply.send(ConsentFlowStatus::Error(
                    "active sampling is disabled".to_owned(),
                ));
                return false;
            }
            if runtime.state == SamplingState::ConsentPending {
                let _ = reply.send(ConsentFlowStatus::Error(
                    SamplerError::FlowAlreadyActive.to_string(),
                ));
                return false;
            }
            let Some(expected) = runtime.display.display.clone() else {
                let _ = reply.send(ConsentFlowStatus::Error(
                    "no sampled display is available".to_owned(),
                ));
                return false;
            };
            if env_reader("WAYLAND_DISPLAY").is_none() && env_reader("DISPLAY").is_none() {
                let _ = reply.send(ConsentFlowStatus::Error(
                    SamplerError::NoGraphicalSession.to_string(),
                ));
                return false;
            }
            let transition = apply_trigger(runtime, Trigger::GrantStarted, status_tx);
            debug_assert!(
                transition.effects.contains(&Effect::OpenConsent),
                "only GrantStarted may enter ConsentPending"
            );
            // The portal has no config timeout; the five-minute interaction bound
            // prevents an abandoned dialog from retaining a daemon operation forever.
            let mut consent = Box::pin(source.request_consent(&expected));
            let mut deadline = Box::pin(tokio::time::sleep(CONSENT_INTERACTION_TIMEOUT));
            let mut pending_disable: Option<(bool, oneshot::Sender<Result<(), SamplerError>>)> =
                None;
            let outcome = loop {
                tokio::select! {
                    () = cancel.cancelled() => break None,
                    () = &mut deadline => break Some(Err(CaptureError::Timeout)),
                    outcome = &mut consent => break Some(outcome),
                    command = command_rx.recv() => match command {
                        Some(SamplerCommand::Enable { reply }) => {
                            let _ = reply.send(ConsentFlowStatus::Error(
                                SamplerError::FlowAlreadyActive.to_string(),
                            ));
                        }
                        Some(SamplerCommand::Disable { forget, reply }) => {
                            apply_trigger(runtime, Trigger::Forget, status_tx);
                            pending_disable = Some((forget, reply));
                            break Some(Err(CaptureError::Protocol("wear_sampling_cancelled".to_owned())));
                        }
                        None => break None,
                    }
                }
            };
            drop(consent);
            drop(deadline);
            if let Some((forget, reply)) = pending_disable {
                source.close().await;
                if forget {
                    if let Err(error) = crate::screencast_consent::forget(consent_path) {
                        let _ = reply.send(Err(SamplerError::Store(error)));
                    } else {
                        runtime.record = None;
                        let _ = reply.send(Ok(()));
                    }
                } else {
                    let _ = reply.send(Ok(()));
                }
            }
            match outcome {
                None => return false,
                Some(Err(CaptureError::Timeout)) => {
                    tracing::warn!(event = "wear_sampling_consent_failed", reason = "timeout");
                    apply_trigger(runtime, Trigger::ConsentTimedOut, status_tx);
                    let _ = reply.send(ConsentFlowStatus::TimedOut);
                }
                Some(Err(CaptureError::ConsentDenied)) => {
                    apply_trigger(runtime, Trigger::ConsentDenied, status_tx);
                    let _ = reply.send(ConsentFlowStatus::Denied);
                }
                Some(Err(error)) => {
                    if matches!(error, CaptureError::Protocol(ref text) if text == "wear_sampling_cancelled")
                    {
                        let _ = reply.send(ConsentFlowStatus::Error("cancelled".to_owned()));
                        return false;
                    }
                    tracing::warn!(
                        event = "wear_sampling_consent_failed",
                        reason = ?error
                    );
                    let trigger = if matches!(error, CaptureError::Protocol(ref text) if text == WEAR_SAMPLING_WRONG_MONITOR)
                    {
                        Trigger::WrongMonitor
                    } else {
                        Trigger::ConsentTimedOut
                    };
                    let transition = apply_trigger(runtime, trigger, status_tx);
                    let reason = transition
                        .effects
                        .iter()
                        .find_map(|effect| match effect {
                            Effect::EnterUniform(reason) => Some(*reason),
                            _ => None,
                        })
                        .unwrap_or(WEAR_SAMPLING_CONSENT_TIMEOUT);
                    let _ = reply.send(ConsentFlowStatus::Error(reason.to_owned()));
                }
                Some(Ok(grant)) => {
                    let record = crate::screencast_consent::ConsentRecord {
                        token: grant.stream.restore_token,
                        sampled_display: expected.display,
                        granted_at: grant.granted_at,
                        portal_persistent_ids: grant.stream.persistent_id.into_iter().collect(),
                        granted_width: grant.stream.frame_width,
                        granted_height: grant.stream.frame_height,
                    };
                    match crate::screencast_consent::store_atomic(consent_path, &record) {
                        Ok(()) => {
                            runtime.record = crate::screencast_consent::load(
                                consent_path,
                                &record.sampled_display,
                            )
                            .ok();
                            tracing::info!(
                                event = "wear_sampling_stage",
                                stage = "token_persisted"
                            );
                            let transition = apply_trigger(runtime, Trigger::Granted, status_tx);
                            let _ = reply.send(ConsentFlowStatus::Granted);
                            return transition.effects.contains(&Effect::Connect);
                        }
                        Err(error) => {
                            apply_trigger(runtime, Trigger::ConsentTimedOut, status_tx);
                            let _ = reply.send(ConsentFlowStatus::Error(error.to_string()));
                        }
                    }
                }
            }
        }
        SamplerCommand::Disable { forget, reply } => {
            let transition = apply_trigger(
                runtime,
                Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                status_tx,
            );
            if transition.effects.contains(&Effect::CloseSession) {
                source.close().await;
            }
            if forget {
                if let Err(error) = crate::screencast_consent::forget(consent_path) {
                    let _ = reply.send(Err(SamplerError::Store(error)));
                    return false;
                }
                runtime.record = None;
            }
            let _ = reply.send(Ok(()));
        }
    }
    false
}

fn apply_update(
    runtime: &mut Runtime,
    update: SamplerUpdate,
    status_tx: &watch::Sender<SamplerStatus>,
) -> Option<Transition> {
    match update {
        SamplerUpdate::Reconfigure(plan) => {
            if plan.trigger == ConfigDelta::StreamModeChanged {
                runtime.pending_stream_reset = true;
            }
            if plan.trigger == ConfigDelta::SampledDisplayChanged {
                runtime.record = None;
            }
            runtime.active = plan.active_sampling;
            runtime.sample_interval = plan.sample_interval;
            Some(apply_trigger(
                runtime,
                Trigger::ConfigChanged(plan.trigger),
                status_tx,
            ))
        }
        SamplerUpdate::DisplayContext(context) => {
            let was_present = runtime.display.display.is_some();
            let is_present = context.display.is_some();
            runtime.display = context;
            (was_present != is_present).then(|| {
                apply_trigger(
                    runtime,
                    Trigger::ConfigChanged(ConfigDelta::DisplayPresent(is_present)),
                    status_tx,
                )
            })
        }
    }
}

async fn apply_update_with_effects(
    runtime: &mut Runtime,
    source: &mut dyn CaptureSource,
    update: SamplerUpdate,
    status_tx: &watch::Sender<SamplerStatus>,
) -> Option<Transition> {
    let transition = apply_update(runtime, update, status_tx);
    if transition
        .as_ref()
        .is_some_and(|transition| transition.effects.contains(&Effect::CloseSession))
    {
        source.close().await;
    }
    transition
}

fn transition_to(
    runtime: &mut Runtime,
    state: SamplingState,
    reason: Option<&'static str>,
    status_tx: &watch::Sender<SamplerStatus>,
) {
    runtime.state = state;
    publish_status(status_tx, runtime, reason, None);
    if let Some(reason) = reason
        && runtime.episode_warned.insert(runtime.display_name())
    {
        tracing::warn!(reason, display = %runtime.display_name(), "active sampling is using uniform attribution");
    }
}

fn apply_trigger(
    runtime: &mut Runtime,
    trigger: Trigger,
    status_tx: &watch::Sender<SamplerStatus>,
) -> Transition {
    let transition = decide(runtime.state, trigger, runtime.record.is_some());
    let reason = transition.effects.iter().find_map(|effect| {
        if let Effect::EnterUniform(reason) = effect {
            Some(*reason)
        } else {
            None
        }
    });
    transition_to(runtime, transition.next, reason, status_tx);
    transition
}

fn publish_status(
    status_tx: &watch::Sender<SamplerStatus>,
    runtime: &Runtime,
    reason: Option<&'static str>,
    last_capture: Option<Tick>,
) {
    let current = status_tx.borrow().clone();
    if let Some(event_tx) = &runtime.event_tx {
        if current.state != SamplingState::Streaming && runtime.state == SamplingState::Streaming {
            let _ = event_tx.try_send(ControlMsg::PublishDaemonEvent(
                DaemonEvent::WearSamplingStarted,
            ));
        }
        if current.uniform_reason.is_none()
            && let Some(reason) = reason
        {
            let _ = event_tx.try_send(ControlMsg::PublishDaemonEvent(
                DaemonEvent::WearSamplingDegraded {
                    reason: reason.to_owned(),
                },
            ));
        }
    }
    status_tx.send_replace(SamplerStatus {
        state: runtime.state,
        last_capture: last_capture.or(current.last_capture),
        uniform_reason: reason,
        bound_display: runtime
            .record
            .as_ref()
            .map(|record| record.record().sampled_display.clone()),
        granted_at: runtime
            .record
            .as_ref()
            .map(|record| record.record().granted_at),
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "the daemon-lifetime loop keeps cancellation and every owned timer in one select-driven state machine"
)]
async fn run(
    mut deps: ActiveSamplerDeps,
    mut command_rx: mpsc::Receiver<SamplerCommand>,
    status_tx: watch::Sender<SamplerStatus>,
) {
    let mut runtime = Runtime::new(&deps.initial_config, &deps.consent_path);
    runtime.event_tx = deps.event_tx.take();
    let initial_reason = match runtime.state {
        SamplingState::NeedsConsent => Some(WEAR_SAMPLING_NEEDS_CONSENT),
        SamplingState::Suspended => Some(WEAR_SAMPLING_SUSPENDED),
        _ => None,
    };
    let initial_state = runtime.state;
    transition_to(&mut runtime, initial_state, initial_reason, &status_tx);
    let mut cadence = cadence_for(&runtime);
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut capture_now = runtime.state == SamplingState::Streaming;

    loop {
        if deps.cancel.is_cancelled() {
            break;
        }

        match runtime.state {
            SamplingState::Disabled | SamplingState::NeedsConsent | SamplingState::Suspended => {
                tokio::select! {
                    () = deps.cancel.cancelled() => break,
                    command = command_rx.recv() => {
                        if let Some(command) = command
                            && handle_command(&mut runtime, &mut *deps.source, &deps.consent_path, command, &mut command_rx, &status_tx, &deps.cancel, deps.env_reader).await {
                            capture_now = runtime.state == SamplingState::Streaming;
                        }
                    }
                    update = deps.update_rx.recv() => {
                        if let Some(update) = update {
                            let _ = apply_update_with_effects(&mut runtime, &mut *deps.source, update, &status_tx).await;
                            cadence = cadence_for(&runtime);
                        }
                    }
                }
            }
            SamplingState::Connecting => {
                let Some(record) = runtime.record.clone() else {
                    apply_trigger(&mut runtime, Trigger::AuthFailed, &status_tx);
                    continue;
                };
                match connect(&mut *deps.source, &record, &deps.cancel).await {
                    ConnectOutcome::Cancelled => break,
                    ConnectOutcome::Connected(stream) => {
                        let transition =
                            apply_trigger(&mut runtime, Trigger::Connected, &status_tx);
                        if transition.effects.contains(&Effect::SaveRotatedToken)
                            && persist_rotated_token(&mut runtime, stream, &deps.consent_path)
                                .is_err()
                        {
                            apply_trigger(&mut runtime, Trigger::AuthFailed, &status_tx);
                            continue;
                        }
                        runtime.reconnect_backoff = Duration::from_secs(30);
                        capture_now = transition.effects.contains(&Effect::Capture);
                    }
                    ConnectOutcome::NeedsConsent(reason) => {
                        let trigger = if reason == WEAR_SAMPLING_WRONG_MONITOR {
                            Trigger::WrongMonitor
                        } else {
                            Trigger::AuthFailed
                        };
                        apply_trigger(&mut runtime, trigger, &status_tx);
                    }
                    ConnectOutcome::Transport => {
                        apply_trigger(&mut runtime, Trigger::TransportFailed, &status_tx);
                        let delay = runtime.reconnect_backoff;
                        runtime.reconnect_backoff =
                            (runtime.reconnect_backoff * 2).min(Duration::from_secs(300));
                        tokio::select! {
                            () = deps.cancel.cancelled() => break,
                            () = tokio::time::sleep(delay) => {},
                            update = deps.update_rx.recv() => if let Some(update) = update { let _ = apply_update_with_effects(&mut runtime, &mut *deps.source, update, &status_tx).await; },
                             command = command_rx.recv() => if let Some(command) = command { let _ = handle_command(&mut runtime, &mut *deps.source, &deps.consent_path, command, &mut command_rx, &status_tx, &deps.cancel, deps.env_reader).await; },
                        }
                    }
                }
            }
            SamplingState::Streaming => {
                if !capture_now {
                    tokio::select! {
                        () = deps.cancel.cancelled() => break,
                        _ = cadence.tick() => {},
                        update = deps.update_rx.recv() => {
                            if let Some(update) = update
                                && let Some(transition) = apply_update_with_effects(&mut runtime, &mut *deps.source, update, &status_tx).await
                            {
                                capture_now |= transition.effects.contains(&Effect::Capture);
                            }
                            continue;
                        }
                        command = command_rx.recv() => {
                            if let Some(command) = command { let _ = handle_command(&mut runtime, &mut *deps.source, &deps.consent_path, command, &mut command_rx, &status_tx, &deps.cancel, deps.env_reader).await; }
                            continue;
                        }
                    }
                }
                capture_now = false;
                let attempt = capture_one(
                    &mut *deps.source,
                    &runtime.active,
                    runtime.display.phase.clone(),
                    &deps.latest_grid,
                    &deps.cancel,
                    &mut cadence,
                    std::mem::take(&mut runtime.pending_stream_reset),
                )
                .await;
                match attempt {
                    CaptureOutcome::Cancelled => break,
                    CaptureOutcome::Ok(captured_at) => {
                        runtime.failures = 0;
                        runtime.episode_warned.clear();
                        apply_trigger(&mut runtime, Trigger::CaptureOk, &status_tx);
                        publish_status(&status_tx, &runtime, None, Some(captured_at));
                    }
                    CaptureOutcome::Failed(error, overlapping) => {
                        runtime.failures = runtime.failures.saturating_add(1 + overlapping);
                        apply_capture_failure(&mut runtime, error, &status_tx);
                    }
                }
            }
            SamplingState::Cooldown => {
                tokio::select! {
                    () = deps.cancel.cancelled() => break,
                    () = tokio::time::sleep(runtime.active.circuit_reset_after) => {
                        let transition = apply_trigger(&mut runtime, Trigger::CooldownElapsed, &status_tx);
                        debug_assert!(transition.effects.contains(&Effect::Capture), "only CooldownElapsed may schedule a cooldown retry");
                        let attempt = capture_one(&mut *deps.source, &runtime.active, runtime.display.phase.clone(), &deps.latest_grid, &deps.cancel, &mut cadence, std::mem::take(&mut runtime.pending_stream_reset)).await;
                        match attempt {
                            CaptureOutcome::Cancelled => break,
                            CaptureOutcome::Ok(captured_at) => { runtime.failures = 0; runtime.episode_warned.clear(); apply_trigger(&mut runtime, Trigger::CaptureOk, &status_tx); publish_status(&status_tx, &runtime, None, Some(captured_at)); }
                            CaptureOutcome::Failed(error, _) => {
                                apply_capture_failure(&mut runtime, error, &status_tx);
                            }
                        }
                    }
                    update = deps.update_rx.recv() => if let Some(update) = update
                        && let Some(transition) = apply_update_with_effects(&mut runtime, &mut *deps.source, update, &status_tx).await {
                        capture_now |= transition.effects.contains(&Effect::Capture);
                    },
                     command = command_rx.recv() => if let Some(command) = command { let _ = handle_command(&mut runtime, &mut *deps.source, &deps.consent_path, command, &mut command_rx, &status_tx, &deps.cancel, deps.env_reader).await; },
                }
            }
            SamplingState::ConsentPending => {
                unreachable!("consent flows run to completion in their command branch")
            }
        }
    }
    deps.source.close().await;
}

impl Runtime {
    fn initial_interval(&self) -> Duration {
        // Cadence intentionally shares the wear tick knob; active sampling has no second interval.
        self.sample_interval
    }
}

fn cadence_for(runtime: &Runtime) -> tokio::time::Interval {
    let interval = runtime.initial_interval();
    let mut cadence = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    cadence
}

/// Lifecycle state of the active sampling service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingState {
    /// Sampling is disabled by configuration.
    Disabled,
    /// Sampling needs an explicit portal consent grant.
    NeedsConsent,
    /// An explicit portal consent request is in progress.
    ConsentPending,
    /// A saved consent token is being reattached.
    Connecting,
    /// The capture session is available for periodic samples.
    Streaming,
    /// Repeated capture failures have opened the circuit breaker.
    Cooldown,
    /// Sampling is administratively paused while retaining consent.
    Suspended,
}

/// Relevant active-sampling configuration change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDelta {
    /// Active sampling was enabled or disabled.
    Enabled(bool),
    /// The configured sampled display changed.
    SampledDisplayChanged,
    /// The capture stream strategy changed.
    StreamModeChanged,
    /// Capture timeout, failure threshold, or cooldown duration changed.
    LimitsChanged,
    /// The global wear tracker was enabled or disabled.
    WearEnabled(bool),
    /// The configured display appeared or disappeared in a new generation.
    DisplayPresent(bool),
}

/// Event consumed by the sampler lifecycle table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A relevant configuration value changed.
    ConfigChanged(ConfigDelta),
    /// An explicit consent flow was requested.
    GrantStarted,
    /// The portal completed an explicit consent flow.
    Granted,
    /// The operator declined a consent request.
    ConsentDenied,
    /// The consent request exceeded its deadline.
    ConsentTimedOut,
    /// A saved consent token successfully connected.
    Connected,
    /// One capture completed successfully.
    CaptureOk,
    /// The failure threshold was reached, or a cooldown retry failed again.
    CaptureFailed,
    /// The portal reported an authentication or permission failure.
    AuthFailed,
    /// The portal transport failed without invalidating consent.
    TransportFailed,
    /// The portal closed the active capture session.
    SessionClosed,
    /// Portal stream metadata or its first frame identifies another monitor.
    WrongMonitor,
    /// The circuit-breaker timer elapsed.
    CooldownElapsed,
    /// The operator requested removal of the consent record.
    Forget,
}

/// Work the asynchronous sampler shell performs after a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Start the explicit portal consent flow.
    OpenConsent,
    /// Reattach to a saved portal consent record.
    Connect,
    /// Request one frame from the current capture session.
    Capture,
    /// Close the active portal session.
    CloseSession,
    /// Cancel an in-flight explicit consent flow.
    CancelConsent,
    /// Attribute uniformly with the supplied stable fallback reason.
    EnterUniform(&'static str),
    /// Persist the token returned by a successful reconnect.
    SaveRotatedToken,
}

/// Result of a pure lifecycle decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// State after the triggering event.
    pub next: SamplingState,
    /// Shell actions needed to realize the transition.
    pub effects: Vec<Effect>,
}

/// Borrowed consent record data needed to open a saved portal stream.
#[derive(Clone, Copy)]
pub struct ConsentBinding<'a> {
    /// Opaque portal restore token.
    pub token: &'a str,
    /// Display selected when the grant was recorded.
    pub sampled_display: &'a str,
    /// Persistent portal identities observed for the granted display.
    pub portal_persistent_ids: &'a [String],
    /// Native width observed in the first frame delivered at grant time.
    pub granted_width: u32,
    /// Native height observed in the first frame delivered at grant time.
    pub granted_height: u32,
}

/// Display identity used to request a fresh portal grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayExpectation {
    /// Configured display identity.
    pub display: String,
}

/// Metadata from a connected portal stream.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectedStream {
    /// Portal-private `PipeWire` node identifier.
    pub node_id: u32,
    /// Token to persist for a subsequent reconnect.
    pub restore_token: String,
    /// Portal persistent identity when the compositor provides one.
    pub persistent_id: Option<String>,
    /// Stream width reported by the portal.
    pub width: u32,
    /// Stream height reported by the portal.
    pub height: u32,
    /// Native width observed in the first delivered `PipeWire` frame.
    pub frame_width: u32,
    /// Native height observed in the first delivered `PipeWire` frame.
    pub frame_height: u32,
}

impl fmt::Debug for ConnectedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectedStream")
            .field("node_id", &self.node_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Successful explicit portal grant.
#[derive(Clone, PartialEq, Eq)]
pub struct Grant {
    /// Stream metadata returned by the portal.
    pub stream: ConnectedStream,
    /// Wall-clock timestamp used only for consent-record status reporting.
    pub granted_at: OffsetDateTime,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field("stream", &self.stream)
            .field("granted_at", &self.granted_at)
            .finish()
    }
}

/// Raw portal frame before privacy-preserving block reduction.
#[derive(Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Packed RGBA bytes, including per-row padding when `stride` exceeds width.
    pub rgba: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Number of bytes between adjacent rows.
    pub stride: usize,
}

impl fmt::Debug for RawFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawFrame")
            .field("rgba_len", &self.rgba.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .finish()
    }
}

/// Error returned by a portal capture operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The operator cancelled an explicit portal consent request.
    ConsentDenied,
    /// The saved grant is invalid or lacks permission.
    Auth,
    /// A recoverable portal or `PipeWire` transport failure.
    Transport(String),
    /// The compositor closed the portal capture session.
    SessionClosed,
    /// An unexpected protocol-level error.
    Protocol(String),
    /// The operation exceeded the configured capture deadline.
    Timeout,
}

/// Asynchronous boundary between the portable sampler and platform capture.
#[async_trait]
pub trait CaptureSource: Send + Sync + 'static {
    /// Reattach to a previously granted portal session.
    async fn connect(
        &mut self,
        binding: &ConsentBinding<'_>,
    ) -> Result<ConnectedStream, CaptureError>;

    /// Request a fresh portal consent grant for one display.
    async fn request_consent(
        &mut self,
        display: &DisplayExpectation,
    ) -> Result<Grant, CaptureError>;

    /// Capture one frame using the configured stream strategy.
    async fn capture_one(&mut self, mode: StreamMode) -> Result<RawFrame, CaptureError>;

    /// Rebuild capture resources while retaining the portal session and token.
    async fn reset_stream(&mut self) {}

    /// Release any live portal session.
    async fn close(&mut self);
}

/// Decide the next lifecycle state without performing portal I/O.
///
/// `has_consent_record` is ambient persistent state: it distinguishes a safe
/// reconnect from a fresh-consent requirement after enabling or resuming.
#[must_use]
pub fn decide(state: SamplingState, trigger: Trigger, has_consent_record: bool) -> Transition {
    match trigger {
        Trigger::ConfigChanged(ConfigDelta::Enabled(false)) => disabled(state),
        Trigger::ConfigChanged(ConfigDelta::SampledDisplayChanged) => invalidate_binding(state),
        Trigger::ConfigChanged(
            ConfigDelta::WearEnabled(false) | ConfigDelta::DisplayPresent(false),
        ) => suspended(state),
        Trigger::ConfigChanged(ConfigDelta::Enabled(true)) if state == SamplingState::Disabled => {
            resume(has_consent_record)
        }
        Trigger::ConfigChanged(
            ConfigDelta::WearEnabled(true) | ConfigDelta::DisplayPresent(true),
        ) if state == SamplingState::Suspended => resume(has_consent_record),
        Trigger::GrantStarted if state == SamplingState::NeedsConsent => {
            transition(SamplingState::ConsentPending, vec![Effect::OpenConsent])
        }
        Trigger::Granted if state == SamplingState::ConsentPending => {
            transition(SamplingState::Connecting, vec![Effect::Connect])
        }
        Trigger::ConsentDenied | Trigger::ConsentTimedOut
            if state == SamplingState::ConsentPending =>
        {
            transition(
                SamplingState::NeedsConsent,
                vec![Effect::EnterUniform(WEAR_SAMPLING_CONSENT_TIMEOUT)],
            )
        }
        Trigger::WrongMonitor if state == SamplingState::ConsentPending => {
            needs_consent(state, WEAR_SAMPLING_WRONG_MONITOR)
        }
        Trigger::Forget if state == SamplingState::ConsentPending => transition(
            SamplingState::NeedsConsent,
            vec![
                Effect::CancelConsent,
                Effect::EnterUniform(WEAR_SAMPLING_NEEDS_CONSENT),
            ],
        ),
        Trigger::Connected if state == SamplingState::Connecting => transition(
            SamplingState::Streaming,
            vec![Effect::SaveRotatedToken, Effect::Capture],
        ),
        Trigger::TransportFailed if state == SamplingState::Connecting => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_PORTAL_UNREACHABLE)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Connecting => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::WrongMonitor if state == SamplingState::Connecting => {
            needs_consent(state, WEAR_SAMPLING_WRONG_MONITOR)
        }
        Trigger::CaptureOk if state == SamplingState::Streaming => transition(state, vec![]),
        Trigger::CaptureFailed if state == SamplingState::Streaming => transition(
            SamplingState::Cooldown,
            vec![Effect::EnterUniform(WEAR_SAMPLING_CAPTURE_FAILED)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Streaming => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::WrongMonitor if state == SamplingState::Streaming => {
            needs_consent(state, WEAR_SAMPLING_WRONG_MONITOR)
        }
        Trigger::TransportFailed if state == SamplingState::Streaming => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_PORTAL_UNREACHABLE)],
        ),
        Trigger::CooldownElapsed if state == SamplingState::Cooldown => {
            transition(SamplingState::Cooldown, vec![Effect::Capture])
        }
        Trigger::CaptureOk if state == SamplingState::Cooldown => {
            transition(SamplingState::Streaming, vec![])
        }
        Trigger::TransportFailed if state == SamplingState::Cooldown => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_PORTAL_UNREACHABLE)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Cooldown => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::WrongMonitor if state == SamplingState::Cooldown => {
            needs_consent(state, WEAR_SAMPLING_WRONG_MONITOR)
        }
        Trigger::CaptureFailed if state == SamplingState::Cooldown => transition(
            SamplingState::Cooldown,
            vec![Effect::EnterUniform(WEAR_SAMPLING_COOLDOWN)],
        ),
        Trigger::ConfigChanged(ConfigDelta::LimitsChanged) if state == SamplingState::Cooldown => {
            transition(SamplingState::Streaming, vec![Effect::Capture])
        }
        _ => transition(state, vec![]),
    }
}

fn disabled(state: SamplingState) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    if state != SamplingState::Disabled {
        effects.push(Effect::CloseSession);
    }
    transition(SamplingState::Disabled, effects)
}

fn needs_consent(state: SamplingState, reason: &'static str) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    effects.push(Effect::EnterUniform(reason));
    transition(SamplingState::NeedsConsent, effects)
}

fn invalidate_binding(state: SamplingState) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    if matches!(
        state,
        SamplingState::Connecting | SamplingState::Streaming | SamplingState::Cooldown
    ) {
        effects.push(Effect::CloseSession);
    }
    effects.push(Effect::EnterUniform(WEAR_SAMPLING_DISPLAY_CHANGED));
    transition(SamplingState::NeedsConsent, effects)
}

fn suspended(state: SamplingState) -> Transition {
    let mut effects = Vec::new();
    if state == SamplingState::ConsentPending {
        effects.push(Effect::CancelConsent);
    }
    if matches!(
        state,
        SamplingState::Connecting | SamplingState::Streaming | SamplingState::Cooldown
    ) {
        effects.push(Effect::CloseSession);
    }
    effects.push(Effect::EnterUniform(WEAR_SAMPLING_SUSPENDED));
    transition(SamplingState::Suspended, effects)
}

fn resume(has_consent_record: bool) -> Transition {
    if has_consent_record {
        transition(SamplingState::Connecting, vec![Effect::Connect])
    } else {
        transition(
            SamplingState::NeedsConsent,
            vec![Effect::EnterUniform(WEAR_SAMPLING_NEEDS_CONSENT)],
        )
    }
}

fn transition(next: SamplingState, effects: Vec<Effect>) -> Transition {
    Transition { next, effects }
}

#[cfg(test)]
use std::collections::VecDeque;

#[cfg(test)]
#[derive(Debug)]
enum ScriptedOutcome<T> {
    Ready(Result<T, CaptureError>),
    Pending,
}

/// Capture boundary fake with deterministic queued outcomes for lifecycle tests.
#[cfg(test)]
#[derive(Debug, Default)]
struct ScriptedCaptureSource {
    connections: VecDeque<ScriptedOutcome<ConnectedStream>>,
    grants: VecDeque<ScriptedOutcome<Grant>>,
    frames: VecDeque<ScriptedOutcome<RawFrame>>,
    close_calls: usize,
}

#[cfg(test)]
impl ScriptedCaptureSource {
    fn with_frames(outcomes: impl IntoIterator<Item = Result<RawFrame, CaptureError>>) -> Self {
        Self {
            frames: outcomes.into_iter().map(ScriptedOutcome::Ready).collect(),
            ..Self::default()
        }
    }

    fn with_pending_capture() -> Self {
        Self {
            frames: VecDeque::from([ScriptedOutcome::Pending]),
            ..Self::default()
        }
    }

    fn with_pending_consent() -> Self {
        Self {
            grants: VecDeque::from([ScriptedOutcome::Pending]),
            ..Self::default()
        }
    }

    fn close_calls(&self) -> usize {
        self.close_calls
    }
}

#[cfg(test)]
async fn next_scripted<T>(queue: &mut VecDeque<ScriptedOutcome<T>>) -> Result<T, CaptureError> {
    match queue.front() {
        Some(ScriptedOutcome::Pending) => std::future::pending().await,
        Some(ScriptedOutcome::Ready(_)) => match queue.pop_front() {
            Some(ScriptedOutcome::Ready(outcome)) => outcome,
            Some(ScriptedOutcome::Pending) | Option::None => unreachable!("checked queued outcome"),
        },
        Option::None => Err(CaptureError::Protocol("script exhausted".to_owned())),
    }
}

#[cfg(test)]
#[async_trait]
impl CaptureSource for ScriptedCaptureSource {
    async fn connect(
        &mut self,
        _binding: &ConsentBinding<'_>,
    ) -> Result<ConnectedStream, CaptureError> {
        next_scripted(&mut self.connections).await
    }

    async fn request_consent(
        &mut self,
        _display: &DisplayExpectation,
    ) -> Result<Grant, CaptureError> {
        next_scripted(&mut self.grants).await
    }

    async fn capture_one(&mut self, _mode: StreamMode) -> Result<RawFrame, CaptureError> {
        next_scripted(&mut self.frames).await
    }

    async fn close(&mut self) {
        self.close_calls += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[allow(clippy::unnecessary_wraps)]
    fn test_env_reader(_name: &str) -> Option<String> {
        Some("test-session".to_owned())
    }

    fn headless_env_reader(_name: &str) -> Option<String> {
        None
    }

    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("trace buffer lock is not poisoned")
                .write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_tracing(
        level: tracing::Level,
    ) -> (
        std::sync::Arc<Mutex<Vec<u8>>>,
        tracing::dispatcher::DefaultGuard,
    ) {
        let buffer = std::sync::Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(level)
            .with_writer(CaptureWriter(buffer.clone()))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (buffer, guard)
    }

    fn traced_output(buffer: &std::sync::Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(
            buffer
                .lock()
                .expect("trace buffer lock is not poisoned")
                .clone(),
        )
        .expect("tracing output is UTF-8")
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_latest_replaces_capture_results() {
        let latest = new_latest_grid();
        replace_latest(
            &latest,
            SampledGrid {
                grid: dormant_core::spatial_grid::LumaGrid::new(vec![0.1; 16 * 9]).unwrap(),
                captured_at: dormant_core::types::Tick::now(),
                phase_at_capture: dormant_core::state_machine::Phase::Active,
            },
        );
        replace_latest(
            &latest,
            SampledGrid {
                grid: dormant_core::spatial_grid::LumaGrid::new(vec![0.9; 16 * 9]).unwrap(),
                captured_at: dormant_core::types::Tick::now(),
                phase_at_capture: dormant_core::state_machine::Phase::Active,
            },
        );

        let sample = latest.read().unwrap().clone().unwrap();
        assert_eq!(sample.grid.cells, vec![0.9; 16 * 9]);
    }

    #[test]
    fn display_removed_on_generation_swap_suspends_a_reattaching_sampler() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));

        let transition = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: None,
                phase: Phase::Active,
                stage_active: false,
            }),
            &status_tx,
        );

        assert_eq!(runtime.state, SamplingState::Suspended);
        assert_eq!(
            transition
                .expect("display removal has a lifecycle transition")
                .effects,
            vec![
                Effect::CloseSession,
                Effect::EnterUniform(WEAR_SAMPLING_SUSPENDED)
            ]
        );

        let restored = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "oled".to_owned(),
                }),
                phase: Phase::Active,
                stage_active: true,
            }),
            &status_tx,
        )
        .expect("display restoration has a lifecycle transition");
        assert_eq!(runtime.state, SamplingState::Connecting);
        assert_eq!(restored.effects, vec![Effect::Connect]);
    }

    #[derive(Clone)]
    enum TestCapture {
        Frame,
        Failure(CaptureError),
        Pending,
    }

    struct ServiceSource {
        connects: Mutex<VecDeque<Result<ConnectedStream, CaptureError>>>,
        connects_seen: Arc<AtomicUsize>,
        captures: Mutex<VecDeque<TestCapture>>,
        captures_seen: Arc<AtomicUsize>,
        closes_seen: Arc<AtomicUsize>,
        grants_seen: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CaptureSource for ServiceSource {
        async fn connect(
            &mut self,
            _binding: &ConsentBinding<'_>,
        ) -> Result<ConnectedStream, CaptureError> {
            self.connects_seen.fetch_add(1, Ordering::SeqCst);
            self.connects
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(test_stream()))
        }

        async fn request_consent(
            &mut self,
            _display: &DisplayExpectation,
        ) -> Result<Grant, CaptureError> {
            self.grants_seen.fetch_add(1, Ordering::SeqCst);
            Ok(Grant {
                stream: test_stream(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
        }

        async fn capture_one(&mut self, _mode: StreamMode) -> Result<RawFrame, CaptureError> {
            self.captures_seen.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .captures
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(TestCapture::Frame);
            match outcome {
                TestCapture::Frame => Ok(test_frame()),
                TestCapture::Failure(error) => Err(error),
                TestCapture::Pending => std::future::pending().await,
            }
        }

        async fn close(&mut self) {
            self.closes_seen.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ReloadSource {
        connects: Arc<AtomicUsize>,
        resets: Arc<AtomicUsize>,
        modes: Arc<Mutex<Vec<StreamMode>>>,
    }

    #[async_trait]
    impl CaptureSource for ReloadSource {
        async fn connect(
            &mut self,
            _binding: &ConsentBinding<'_>,
        ) -> Result<ConnectedStream, CaptureError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(test_stream())
        }

        async fn request_consent(
            &mut self,
            _display: &DisplayExpectation,
        ) -> Result<Grant, CaptureError> {
            Err(CaptureError::Protocol(
                "unexpected consent request".to_owned(),
            ))
        }

        async fn capture_one(&mut self, mode: StreamMode) -> Result<RawFrame, CaptureError> {
            self.modes.lock().unwrap().push(mode);
            Ok(test_frame())
        }

        async fn reset_stream(&mut self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
        }

        async fn close(&mut self) {}
    }

    fn test_stream() -> ConnectedStream {
        ConnectedStream {
            node_id: 1,
            restore_token: "rotated".to_owned(),
            persistent_id: Some("test-panel".to_owned()),
            width: 16,
            height: 9,
            frame_width: 16,
            frame_height: 9,
        }
    }

    fn test_frame() -> RawFrame {
        RawFrame {
            rgba: vec![128; 16 * 9 * 4],
            width: 16,
            height: 9,
            stride: 16 * 4,
        }
    }

    fn active_config(interval: Duration) -> Arc<Config> {
        let mut config = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: dormant_core::config::schema::DaemonConfig::default(),
            sensors: indexmap::IndexMap::new(),
            zones: indexmap::IndexMap::new(),
            displays: indexmap::IndexMap::new(),
            rules: indexmap::IndexMap::new(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        };
        config.wear.enabled = true;
        config.wear.sample_interval = interval;
        config.wear.active_sampling.enabled = true;
        config.wear.active_sampling.sampled_display = Some("oled".to_owned());
        config.wear.active_sampling.capture_timeout = Duration::from_secs(2);
        config.wear.active_sampling.failure_threshold = 1;
        config.wear.active_sampling.circuit_reset_after = Duration::from_secs(5);
        Arc::new(config)
    }

    fn test_record(path: &std::path::Path) {
        crate::screencast_consent::store_atomic(
            path,
            &crate::screencast_consent::ConsentRecord {
                token: "saved".to_owned(),
                sampled_display: "oled".to_owned(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
                portal_persistent_ids: vec!["test-panel".to_owned()],
                granted_width: 16,
                granted_height: 9,
            },
        )
        .unwrap();
    }

    fn service_deps(
        config: Arc<Config>,
        source: ServiceSource,
        consent_path: PathBuf,
        cancel: CancellationToken,
    ) -> (ActiveSamplerDeps, mpsc::Sender<SamplerUpdate>, LatestGrid) {
        let latest_grid = new_latest_grid();
        let (update_tx, update_rx) = mpsc::channel(2);
        (
            ActiveSamplerDeps {
                initial_config: config,
                update_rx,
                latest_grid: latest_grid.clone(),
                source: Box::new(source),
                consent_path,
                cancel,
                env_reader: test_env_reader,
                event_tx: None,
            },
            update_tx,
            latest_grid,
        )
    }

    #[test]
    fn streaming_entry_emits_started_once() {
        let dir = tempdir().unwrap();
        let config = active_config(Duration::from_secs(10));
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let mut runtime = Runtime::new(&config, &dir.path().join("consent.json"));
        runtime.event_tx = Some(event_tx);
        let (status_tx, _) = watch::channel(initial_status(&config));

        transition_to(&mut runtime, SamplingState::Streaming, None, &status_tx);
        publish_status(&status_tx, &runtime, None, None);

        assert!(matches!(
            event_rx.try_recv(),
            Ok(ControlMsg::PublishDaemonEvent(
                DaemonEvent::WearSamplingStarted
            ))
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn degradation_events_latch_and_rearm_after_streaming() {
        let dir = tempdir().unwrap();
        let config = active_config(Duration::from_secs(10));
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let mut runtime = Runtime::new(&config, &dir.path().join("consent.json"));
        runtime.event_tx = Some(event_tx);
        let (status_tx, _) = watch::channel(SamplerStatus {
            state: SamplingState::Disabled,
            last_capture: None,
            uniform_reason: None,
            bound_display: None,
            granted_at: None,
        });

        transition_to(
            &mut runtime,
            SamplingState::NeedsConsent,
            Some(WEAR_SAMPLING_NEEDS_CONSENT),
            &status_tx,
        );
        publish_status(
            &status_tx,
            &runtime,
            Some(WEAR_SAMPLING_NEEDS_CONSENT),
            None,
        );
        transition_to(&mut runtime, SamplingState::Streaming, None, &status_tx);
        transition_to(
            &mut runtime,
            SamplingState::Cooldown,
            Some(WEAR_SAMPLING_COOLDOWN),
            &status_tx,
        );

        let reasons: Vec<String> = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                ControlMsg::PublishDaemonEvent(DaemonEvent::WearSamplingDegraded { reason }) => {
                    Some(reason)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                WEAR_SAMPLING_NEEDS_CONSENT.to_owned(),
                WEAR_SAMPLING_COOLDOWN.to_owned()
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_cadence_runs_once_per_interval_and_replaces_latest() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let closes_seen = Arc::new(AtomicUsize::new(0));
        let grants_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::from([TestCapture::Frame, TestCapture::Frame])),
            captures_seen: captures_seen.clone(),
            closes_seen,
            grants_seen,
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, latest) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(captures_seen.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert_eq!(captures_seen.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(captures_seen.load(Ordering::SeqCst), 2);
        assert!(latest.read().unwrap().is_some());

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_stream_mode_reload_resets_only_the_next_capture_without_reconnecting() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let resets_seen = Arc::new(AtomicUsize::new(0));
        let modes_seen = Arc::new(Mutex::new(Vec::new()));
        let source = ReloadSource {
            connects: connects_seen.clone(),
            resets: resets_seen.clone(),
            modes: modes_seen.clone(),
        };
        let cancel = CancellationToken::new();
        let config = active_config(Duration::from_secs(10));
        let (updates_tx, updates_rx) = mpsc::channel(2);
        let deps = ActiveSamplerDeps {
            initial_config: config.clone(),
            update_rx: updates_rx,
            latest_grid: new_latest_grid(),
            source: Box::new(source),
            consent_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(connects_seen.load(Ordering::SeqCst), 1);
        assert_eq!(*modes_seen.lock().unwrap(), vec![StreamMode::Warm]);

        let mut reconfigured = (*config).clone();
        reconfigured.wear.active_sampling.stream_mode = StreamMode::PerTick;
        updates_tx
            .send(SamplerUpdate::Reconfigure(ReconfigurePlan {
                active_sampling: reconfigured.wear.active_sampling,
                sample_interval: reconfigured.wear.sample_interval,
                trigger: ConfigDelta::StreamModeChanged,
            }))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert_eq!(resets_seen.load(Ordering::SeqCst), 1);
        assert_eq!(connects_seen.load(Ordering::SeqCst), 1);
        assert_eq!(
            *modes_seen.lock().unwrap(),
            vec![StreamMode::Warm, StreamMode::PerTick]
        );

        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "oled".to_owned(),
                }),
                phase: Phase::Active,
                stage_active: true,
            }))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert_eq!(resets_seen.load(Ordering::SeqCst), 1);
        assert_eq!(connects_seen.load(Ordering::SeqCst), 1);
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_restart_reattaches_a_retained_consent_record_without_a_grant() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let grants_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::from([Ok(test_stream())])),
            connects_seen: connects_seen.clone(),
            captures: Mutex::new(VecDeque::from([TestCapture::Frame])),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: grants_seen.clone(),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(connects_seen.load(Ordering::SeqCst), 1);
        assert_eq!(grants_seen.load(Ordering::SeqCst), 0);
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_timeout_enters_cooldown_and_cancellation_closes_source() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let closes_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::from([TestCapture::Pending])),
            captures_seen,
            closes_seen: closes_seen.clone(),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(handle.status().borrow().state, SamplingState::Cooldown);
        cancel.cancel();
        join.await.unwrap();
        assert_eq!(closes_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_skips_overlapping_cadence_without_second_capture() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::from([TestCapture::Pending])),
            captures_seen: captures_seen.clone(),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let mut config = active_config(Duration::from_secs(1));
        Arc::get_mut(&mut config)
            .unwrap()
            .wear
            .active_sampling
            .capture_timeout = Duration::from_secs(5);
        let (deps, _updates, _) = service_deps(config, source, consent_path, cancel.clone());
        let (handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(captures_seen.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(handle.status().borrow().state, SamplingState::Cooldown);
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_sigterm_cancels_pending_capture_without_waiting_for_timeout() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let closes_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::from([TestCapture::Pending])),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: closes_seen.clone(),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        cancel.cancel();
        join.await.unwrap();
        assert_eq!(closes_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_cooldown_retries_and_recovers_automatically() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::from([
                TestCapture::Failure(CaptureError::Timeout),
                TestCapture::Frame,
            ])),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(handle.status().borrow().state, SamplingState::Cooldown);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(handle.status().borrow().state, SamplingState::Streaming);
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_reconnects_after_exponential_backoff() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::from([
                Err(CaptureError::Transport("portal unavailable".to_owned())),
                Ok(test_stream()),
            ])),
            connects_seen: connects_seen.clone(),
            captures: Mutex::new(VecDeque::new()),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(connects_seen.load(Ordering::SeqCst), 1);
        assert_eq!(handle.status().borrow().state, SamplingState::Connecting);
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert_eq!(connects_seen.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(connects_seen.load(Ordering::SeqCst), 2);
        assert_eq!(handle.status().borrow().state, SamplingState::Streaming);
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_reconnect_backoff_doubles_then_caps_at_five_minutes() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::from([
                Err(CaptureError::Transport("first".to_owned())),
                Err(CaptureError::Transport("second".to_owned())),
                Err(CaptureError::Transport("third".to_owned())),
                Err(CaptureError::Transport("fourth".to_owned())),
                Err(CaptureError::Transport("fifth".to_owned())),
                Ok(test_stream()),
            ])),
            connects_seen: connects_seen.clone(),
            captures: Mutex::new(VecDeque::new()),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path,
            cancel.clone(),
        );
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        for (before_deadline, expected_attempts, final_second) in [
            (29, 1, 2),
            (59, 2, 3),
            (119, 3, 4),
            (239, 4, 5),
            (299, 5, 6),
        ] {
            tokio::time::advance(Duration::from_secs(before_deadline)).await;
            tokio::task::yield_now().await;
            assert_eq!(connects_seen.load(Ordering::SeqCst), expected_attempts);
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(connects_seen.load(Ordering::SeqCst), final_second);
        }
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_persists_the_rotated_reattach_token() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::from([Ok(test_stream())])),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::new()),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path.clone(),
            cancel.clone(),
        );
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(
            crate::screencast_consent::load(&consent_path, "oled")
                .unwrap()
                .record()
                .token,
            "rotated"
        );
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_opens_consent_only_for_explicit_enable_command() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let grants_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::new()),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: grants_seen.clone(),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _) = service_deps(
            active_config(Duration::from_secs(10)),
            source,
            consent_path.clone(),
            cancel.clone(),
        );
        let (handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        assert_eq!(grants_seen.load(Ordering::SeqCst), 0);
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Enable { reply: reply_tx })
            .await
            .unwrap();
        assert_eq!(reply_rx.await.unwrap(), ConsentFlowStatus::Granted);
        assert_eq!(grants_seen.load(Ordering::SeqCst), 1);
        assert!(consent_path.exists());
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_rejects_a_second_enable_while_consent_is_pending() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        apply_trigger(&mut runtime, Trigger::GrantStarted, &status_tx);
        let mut source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::new()),
            captures_seen: Arc::new(AtomicUsize::new(0)),
            closes_seen: Arc::new(AtomicUsize::new(0)),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        assert!(
            !handle_command(
                &mut runtime,
                &mut source,
                &consent_path,
                SamplerCommand::Enable { reply: reply_tx },
                &mut command_rx,
                &status_tx,
                &CancellationToken::new(),
                test_env_reader,
            )
            .await
        );
        assert_eq!(
            reply_rx.await.unwrap(),
            ConsentFlowStatus::Error(SamplerError::FlowAlreadyActive.to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_rejects_inline_second_enable_while_first_flow_waits() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let (update_tx, update_rx) = mpsc::channel(1);
        drop(update_tx);
        let cancel = CancellationToken::new();
        let (handle, join) = spawn_with_handle(ActiveSamplerDeps {
            initial_config: config,
            update_rx,
            latest_grid: new_latest_grid(),
            source: Box::new(ScriptedCaptureSource::with_pending_consent()),
            consent_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        });

        let (first_tx, _first_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Enable { reply: first_tx })
            .await
            .unwrap();
        tokio::task::yield_now().await;

        let (second_tx, second_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Enable { reply: second_tx })
            .await
            .unwrap();
        assert_eq!(
            second_rx.await.unwrap(),
            ConsentFlowStatus::Error(SamplerError::FlowAlreadyActive.to_string())
        );

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn active_sampler_rejects_enable_without_graphical_session() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let initial_state = runtime.state;
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource::with_pending_consent();
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            headless_env_reader,
        )
        .await;

        assert_eq!(
            reply_rx.await.unwrap(),
            ConsentFlowStatus::Error(SamplerError::NoGraphicalSession.to_string())
        );
        assert_eq!(runtime.state, initial_state);
        assert_eq!(source.close_calls(), 0);
        assert!(!consent_path.exists());
    }

    #[tokio::test]
    async fn active_sampler_config_disabled_replies_without_opening_consent() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let mut config = (*active_config(Duration::from_secs(10))).clone();
        config.wear.active_sampling.enabled = false;
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource::default();
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert_eq!(
            reply_rx.await.unwrap(),
            ConsentFlowStatus::Error("active sampling is disabled".to_owned())
        );
        assert!(!consent_path.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_consent_timeout_replies_timed_out() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource::with_pending_consent();
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let (buffer, _guard) = capture_tracing(tracing::Level::WARN);
        let future = handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &cancel,
            test_env_reader,
        );
        tokio::pin!(future);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(300)).await;
        future.await;

        assert_eq!(reply_rx.await.unwrap(), ConsentFlowStatus::TimedOut);
        let log = traced_output(&buffer);
        assert!(log.contains("wear_sampling_consent_failed"), "{log}");
        assert!(log.contains("reason=\"timeout\""), "{log}");
    }

    #[tokio::test]
    async fn active_sampler_denial_replies_denied() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource {
            grants: VecDeque::from([ScriptedOutcome::Ready(Err(CaptureError::ConsentDenied))]),
            ..ScriptedCaptureSource::default()
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert_eq!(reply_rx.await.unwrap(), ConsentFlowStatus::Denied);
        assert!(!consent_path.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_sampler_logs_consent_failure_reason() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource {
            grants: VecDeque::from([ScriptedOutcome::Ready(Err(CaptureError::Transport(
                "open_pipewire_remote_timeout".to_owned(),
            )))]),
            ..ScriptedCaptureSource::default()
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);
        let (buffer, _guard) = capture_tracing(tracing::Level::WARN);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert_eq!(
            reply_rx.await.unwrap(),
            ConsentFlowStatus::Error(WEAR_SAMPLING_CONSENT_TIMEOUT.to_owned())
        );
        let log = traced_output(&buffer);
        assert!(log.contains("wear_sampling_consent_failed"), "{log}");
        assert!(log.contains("open_pipewire_remote_timeout"), "{log}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_sampler_logs_token_persisted_without_token_value() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut stream = test_stream();
        stream.restore_token = "unlogged-rotated-token".to_owned();
        let mut source = ScriptedCaptureSource {
            grants: VecDeque::from([ScriptedOutcome::Ready(Ok(Grant {
                stream,
                granted_at: OffsetDateTime::UNIX_EPOCH,
            }))]),
            ..ScriptedCaptureSource::default()
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);
        let (buffer, _guard) = capture_tracing(tracing::Level::INFO);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert_eq!(reply_rx.await.unwrap(), ConsentFlowStatus::Granted);
        let log = traced_output(&buffer);
        for stage in ["wear_sampling_stage", "token_persisted"] {
            assert!(log.contains(stage), "missing {stage} stage: {log}");
        }
        assert!(!log.contains("unlogged-rotated-token"), "{log}");
    }

    #[tokio::test]
    async fn active_sampler_fresh_grant_persists_native_dimensions_that_reconcile() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut stream = test_stream();
        stream.width = 3072;
        stream.height = 1728;
        stream.frame_width = 3840;
        stream.frame_height = 2160;
        let frame = RawFrame {
            rgba: vec![0; 4],
            width: 3840,
            height: 2160,
            stride: 3840 * 4,
        };
        let mut source = ScriptedCaptureSource {
            grants: VecDeque::from([ScriptedOutcome::Ready(Ok(Grant {
                stream,
                granted_at: OffsetDateTime::UNIX_EPOCH,
            }))]),
            ..ScriptedCaptureSource::default()
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert_eq!(reply_rx.await.unwrap(), ConsentFlowStatus::Granted);
        let record = crate::screencast_consent::load(&consent_path, "oled")
            .expect("fresh grant record loads");
        assert_eq!(
            (
                record.record().granted_width,
                record.record().granted_height
            ),
            (3840, 2160)
        );
        assert_eq!(
            linux::reconcile_reattached_frame(&frame, &record.as_binding()),
            Ok(())
        );
    }

    #[tokio::test]
    async fn active_sampler_disable_without_forget_retains_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource::default();
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Disable {
                forget: false,
                reply: reply_tx,
            },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert!(matches!(reply_rx.await.unwrap(), Ok(())));
        assert!(crate::screencast_consent::load(&consent_path, "oled").is_ok());
        assert_eq!(source.close_calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_forget_cancels_pending_consent_and_deletes_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let (update_tx, update_rx) = mpsc::channel(1);
        drop(update_tx);
        let cancel = CancellationToken::new();
        let (handle, join) = spawn_with_handle(ActiveSamplerDeps {
            initial_config: config,
            update_rx,
            latest_grid: new_latest_grid(),
            source: Box::new(ScriptedCaptureSource::with_pending_consent()),
            consent_path: consent_path.clone(),
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        });

        let (enable_tx, enable_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Enable { reply: enable_tx })
            .await
            .unwrap();
        tokio::task::yield_now().await;

        let (disable_tx, disable_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Disable {
                forget: true,
                reply: disable_tx,
            })
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(1), disable_rx)
                .await
                .is_ok()
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), enable_rx)
                .await
                .unwrap()
                .unwrap(),
            ConsentFlowStatus::Error("cancelled".to_owned())
        );
        assert!(!consent_path.exists());
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn active_sampler_wrong_monitor_grant_returns_error_without_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path);
        let (status_tx, _) = watch::channel(initial_status(&config));
        let mut source = ScriptedCaptureSource {
            grants: VecDeque::from([ScriptedOutcome::Ready(Err(CaptureError::Protocol(
                WEAR_SAMPLING_WRONG_MONITOR.to_owned(),
            )))]),
            ..ScriptedCaptureSource::default()
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let (_command_tx, mut command_rx) = mpsc::channel(1);

        handle_command(
            &mut runtime,
            &mut source,
            &consent_path,
            SamplerCommand::Enable { reply: reply_tx },
            &mut command_rx,
            &status_tx,
            &CancellationToken::new(),
            test_env_reader,
        )
        .await;

        assert_eq!(
            reply_rx.await.unwrap(),
            ConsentFlowStatus::Error(WEAR_SAMPLING_WRONG_MONITOR.to_owned())
        );
        assert!(!consent_path.exists());
    }

    struct TransitionCase {
        name: &'static str,
        state: SamplingState,
        trigger: Trigger,
        has_consent_record: bool,
        next: SamplingState,
        effects: Vec<Effect>,
    }

    #[test]
    // The full table stays together so state×trigger coverage remains auditable.
    #[allow(clippy::too_many_lines)]
    fn decide_covers_every_specified_lifecycle_exit() {
        let cases = [
            TransitionCase {
                name: "disabled enabling without a record requires consent",
                state: SamplingState::Disabled,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(true)),
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_needs_consent")],
            },
            TransitionCase {
                name: "disabled enabling with a record reconnects",
                state: SamplingState::Disabled,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(true)),
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "needs consent starts only an explicit grant flow",
                state: SamplingState::NeedsConsent,
                trigger: Trigger::GrantStarted,
                has_consent_record: false,
                next: SamplingState::ConsentPending,
                effects: vec![Effect::OpenConsent],
            },
            TransitionCase {
                name: "pending grant connects after a grant",
                state: SamplingState::ConsentPending,
                trigger: Trigger::Granted,
                has_consent_record: false,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "pending consent denial returns to uniform consent fallback",
                state: SamplingState::ConsentPending,
                trigger: Trigger::ConsentDenied,
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_consent_timeout")],
            },
            TransitionCase {
                name: "pending consent timeout returns to uniform consent fallback",
                state: SamplingState::ConsentPending,
                trigger: Trigger::ConsentTimedOut,
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_consent_timeout")],
            },
            TransitionCase {
                name: "forget cancels a pending consent request",
                state: SamplingState::ConsentPending,
                trigger: Trigger::Forget,
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CancelConsent,
                    Effect::EnterUniform("wear_sampling_needs_consent"),
                ],
            },
            TransitionCase {
                name: "connection success starts capture and persists a rotated token",
                state: SamplingState::Connecting,
                trigger: Trigger::Connected,
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![Effect::SaveRotatedToken, Effect::Capture],
            },
            TransitionCase {
                name: "portal unreachable keeps reconnecting with a tagged uniform fallback",
                state: SamplingState::Connecting,
                trigger: Trigger::TransportFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::EnterUniform("wear_sampling_portal_unreachable")],
            },
            TransitionCase {
                name: "token rejection requires fresh consent",
                state: SamplingState::Connecting,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "wrong reattached monitor requires fresh consent with its distinct reason",
                state: SamplingState::Connecting,
                trigger: Trigger::WrongMonitor,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_wrong_monitor")],
            },
            TransitionCase {
                name: "stream capture failure opens the breaker",
                state: SamplingState::Streaming,
                trigger: Trigger::CaptureFailed,
                has_consent_record: true,
                next: SamplingState::Cooldown,
                effects: vec![Effect::EnterUniform("wear_sampling_capture_failed")],
            },
            TransitionCase {
                name: "stream session closure invalidates consent",
                state: SamplingState::Streaming,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "wrong streaming monitor requires fresh consent with its distinct reason",
                state: SamplingState::Streaming,
                trigger: Trigger::WrongMonitor,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_wrong_monitor")],
            },
            TransitionCase {
                name: "stream transport failure reconnects without discarding consent",
                state: SamplingState::Streaming,
                trigger: Trigger::TransportFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::EnterUniform("wear_sampling_portal_unreachable")],
            },
            TransitionCase {
                name: "cooldown expiry retries capture on the existing session",
                state: SamplingState::Cooldown,
                trigger: Trigger::CooldownElapsed,
                has_consent_record: true,
                next: SamplingState::Cooldown,
                effects: vec![Effect::Capture],
            },
            TransitionCase {
                name: "cooldown capture success resumes streaming",
                state: SamplingState::Cooldown,
                trigger: Trigger::CaptureOk,
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![],
            },
            TransitionCase {
                name: "cooldown transport failure reconnects",
                state: SamplingState::Cooldown,
                trigger: Trigger::TransportFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::EnterUniform("wear_sampling_portal_unreachable")],
            },
            TransitionCase {
                name: "cooldown auth failure requires fresh consent",
                state: SamplingState::Cooldown,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "renewed cooldown failure restarts the tagged cooldown",
                state: SamplingState::Cooldown,
                trigger: Trigger::CaptureFailed,
                has_consent_record: true,
                next: SamplingState::Cooldown,
                effects: vec![Effect::EnterUniform("wear_sampling_cooldown")],
            },
            TransitionCase {
                name: "reload resets cooldown and resumes capture",
                state: SamplingState::Cooldown,
                trigger: Trigger::ConfigChanged(ConfigDelta::LimitsChanged),
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![Effect::Capture],
            },
            TransitionCase {
                name: "wear re-enable resumes a retained consent record",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::WearEnabled(true)),
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "wear re-enable without a record stays outside the consent flow",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::WearEnabled(true)),
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_needs_consent")],
            },
            TransitionCase {
                name: "restored display resumes a retained consent record",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::DisplayPresent(true)),
                has_consent_record: true,
                next: SamplingState::Connecting,
                effects: vec![Effect::Connect],
            },
            TransitionCase {
                name: "restored display without a record remains uniform",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::DisplayPresent(true)),
                has_consent_record: false,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_needs_consent")],
            },
            TransitionCase {
                name: "enabled config turns every active state off and closes the session",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "display changes invalidate the consent binding",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::SampledDisplayChanged),
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_display_changed"),
                ],
            },
            TransitionCase {
                name: "wear disable suspends sampling and retains the record",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::WearEnabled(false)),
                has_consent_record: true,
                next: SamplingState::Suspended,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_suspended"),
                ],
            },
            TransitionCase {
                name: "missing display suspends sampling and retains the record",
                state: SamplingState::Streaming,
                trigger: Trigger::ConfigChanged(ConfigDelta::DisplayPresent(false)),
                has_consent_record: true,
                next: SamplingState::Suspended,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_suspended"),
                ],
            },
            TransitionCase {
                name: "stream capture success preserves streaming without a fallback effect",
                state: SamplingState::Streaming,
                trigger: Trigger::CaptureOk,
                has_consent_record: true,
                next: SamplingState::Streaming,
                effects: vec![],
            },
            TransitionCase {
                name: "connecting session closure invalidates consent",
                state: SamplingState::Connecting,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "cooldown session closure invalidates consent",
                state: SamplingState::Cooldown,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![Effect::EnterUniform("wear_sampling_token_invalid")],
            },
            TransitionCase {
                name: "disabling a pending flow cancels consent before closing the session",
                state: SamplingState::ConsentPending,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: false,
                next: SamplingState::Disabled,
                effects: vec![Effect::CancelConsent, Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling while waiting for consent closes the sampler boundary",
                state: SamplingState::NeedsConsent,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: false,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling while reconnecting closes the sampler boundary",
                state: SamplingState::Connecting,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling during cooldown closes the sampler boundary",
                state: SamplingState::Cooldown,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
            TransitionCase {
                name: "disabling a suspended sampler stays disabled",
                state: SamplingState::Suspended,
                trigger: Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                has_consent_record: true,
                next: SamplingState::Disabled,
                effects: vec![Effect::CloseSession],
            },
        ];

        for case in cases {
            let transition = decide(case.state, case.trigger, case.has_consent_record);
            assert_eq!(transition.next, case.next, "{} next state", case.name);
            assert_eq!(transition.effects, case.effects, "{} effects", case.name);
        }
    }

    #[test]
    fn decide_pins_every_uniform_reason_literal() {
        assert_eq!(WEAR_SAMPLING_NEEDS_CONSENT, "wear_sampling_needs_consent");
        assert_eq!(
            WEAR_SAMPLING_CONSENT_TIMEOUT,
            "wear_sampling_consent_timeout"
        );
        assert_eq!(
            WEAR_SAMPLING_PORTAL_UNREACHABLE,
            "wear_sampling_portal_unreachable"
        );
        assert_eq!(WEAR_SAMPLING_TOKEN_INVALID, "wear_sampling_token_invalid");
        assert_eq!(
            WEAR_SAMPLING_DISPLAY_CHANGED,
            "wear_sampling_display_changed"
        );
        assert_eq!(WEAR_SAMPLING_WRONG_MONITOR, "wear_sampling_wrong_monitor");
        assert_eq!(WEAR_SAMPLING_CAPTURE_FAILED, "wear_sampling_capture_failed");
        assert_eq!(WEAR_SAMPLING_COOLDOWN, "wear_sampling_cooldown");
        assert_eq!(WEAR_SAMPLING_SUSPENDED, "wear_sampling_suspended");
    }

    #[test]
    fn decide_keeps_spec_silent_triggers_inert() {
        let cases = [
            (
                "suspended grant start never enters the consent flow",
                SamplingState::Suspended,
                Trigger::GrantStarted,
                true,
            ),
            (
                "suspended grant completion never enters the consent flow",
                SamplingState::Suspended,
                Trigger::Granted,
                true,
            ),
            (
                "disabled capture success is inert",
                SamplingState::Disabled,
                Trigger::CaptureOk,
                false,
            ),
            (
                "needs consent cooldown expiry is inert",
                SamplingState::NeedsConsent,
                Trigger::CooldownElapsed,
                false,
            ),
            (
                "streaming grant start is inert",
                SamplingState::Streaming,
                Trigger::GrantStarted,
                true,
            ),
        ];

        for (name, state, trigger, has_consent_record) in cases {
            let transition = decide(state, trigger, has_consent_record);
            assert_eq!(transition.next, state, "{name} next state");
            assert_eq!(transition.effects, Vec::<Effect>::new(), "{name} effects");
        }
    }

    #[tokio::test]
    async fn scripted_capture_outcomes_are_consumed_in_order() {
        let mut source = ScriptedCaptureSource::with_frames([
            Ok(RawFrame {
                rgba: vec![0, 0, 0, 255],
                width: 1,
                height: 1,
                stride: 4,
            }),
            Err(CaptureError::Auth),
            Err(CaptureError::Transport("portal reset".to_owned())),
            Err(CaptureError::SessionClosed),
        ]);

        assert!(source.capture_one(StreamMode::Warm).await.is_ok());
        assert_eq!(
            source.capture_one(StreamMode::Warm).await,
            Err(CaptureError::Auth)
        );
        assert_eq!(
            source.capture_one(StreamMode::Warm).await,
            Err(CaptureError::Transport("portal reset".to_owned()))
        );
        assert_eq!(
            source.capture_one(StreamMode::Warm).await,
            Err(CaptureError::SessionClosed)
        );
    }

    #[tokio::test]
    async fn scripted_capture_grants_and_connections_are_consumed_in_order() {
        let stream = ConnectedStream {
            node_id: 7,
            restore_token: "rotated".to_owned(),
            persistent_id: Some("panel-7".to_owned()),
            width: 1920,
            height: 1080,
            frame_width: 1920,
            frame_height: 1080,
        };
        let grant = Grant {
            stream: stream.clone(),
            granted_at: OffsetDateTime::UNIX_EPOCH,
        };
        let mut source = ScriptedCaptureSource {
            connections: VecDeque::from([ScriptedOutcome::Ready(Ok(stream.clone()))]),
            grants: VecDeque::from([ScriptedOutcome::Ready(Ok(grant.clone()))]),
            ..ScriptedCaptureSource::default()
        };
        let binding = ConsentBinding {
            token: "saved",
            sampled_display: "oled",
            portal_persistent_ids: &[],
            granted_width: 1920,
            granted_height: 1080,
        };
        let display = DisplayExpectation {
            display: "oled".to_owned(),
        };

        assert_eq!(source.connect(&binding).await, Ok(stream));
        assert_eq!(source.request_consent(&display).await, Ok(grant));
    }

    #[tokio::test]
    async fn scripted_capture_pending_future_can_be_cancelled_before_close() {
        let mut source = ScriptedCaptureSource::with_pending_capture();
        {
            let capture = source.capture_one(StreamMode::PerTick);
            tokio::pin!(capture);

            let waker = std::task::Waker::noop();
            let mut context = std::task::Context::from_waker(waker);
            assert!(matches!(
                std::future::Future::poll(capture.as_mut(), &mut context),
                std::task::Poll::Pending
            ));
        }

        source.close().await;
        assert_eq!(source.close_calls(), 1);
    }
}
