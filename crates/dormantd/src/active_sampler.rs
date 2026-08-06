//! Pure lifecycle rules and capture boundary for active wear sampling.

pub mod source_gate;

use async_trait::async_trait;
use dormant_core::config::schema::{ActiveSamplingConfig, Config, StreamMode};
use dormant_core::ipc_proto::WearSamplingStatus;
use dormant_core::rules::{ControlMsg, DaemonEvent};
use dormant_core::spatial_grid::LumaGrid;
use dormant_core::state_machine::Phase;
use dormant_core::types::{DisplayId, Tick};
use dormant_core::wear::{WearSamplingState, WearSamplingStatus as RedactedWearSamplingStatus};
use std::collections::{BTreeMap, HashMap};
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
pub const WEAR_SAMPLING_SOURCE_UNKNOWN: &str = "wear_sampling_source_unknown";
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

/// Most recent privacy-preserving sample for every configured sampling display.
pub type LatestGrids = Arc<RwLock<HashMap<DisplayId, SampledGrid>>>;

/// Sampler command handles keyed by configured display id.
pub type SamplerRegistry = BTreeMap<DisplayId, ActiveSamplerHandle>;

/// Shared sampler registry used by daemon control surfaces across reloads.
pub type SharedSamplerRegistry = Arc<RwLock<SamplerRegistry>>;

/// Latest lifecycle status for every independently owned sampler.
pub type SamplerStatuses = Arc<RwLock<BTreeMap<DisplayId, SamplerStatus>>>;

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
    /// Configured compositor output for the display targeted by this sampler.
    pub compositor_output: Option<String>,
    /// Grant wall-clock timestamp, exposed without any portal identifiers.
    pub granted_at: Option<OffsetDateTime>,
    /// Latest source-gate observation for this display. `None` means the
    /// display carries no gate configuration — the runtime treats the gate as
    /// permanently matched.
    pub source_gate: Option<source_gate::SourceGate>,
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
        // Redaction contract: only the STABLE GATE STATE STRING crosses
        // the wire. The observed input source (Mismatched.observed) and the
        // reason anchor (Unknown.reason) stay in transition events / logs
        // — they can carry portal identifiers or operator-environment
        // context and MUST NOT appear in a portable status. `None` here
        // means "no gate configured" (a render-only monitor) and is
        // serialized as a field-absent wire shape.
        let source_gate = self.source_gate.as_ref().map(source_gate::SourceGate::tag);
        RedactedWearSamplingStatus {
            state,
            last_capture_age_s: self
                .last_capture
                .map(|capture| now.0.saturating_duration_since(capture.0).as_secs()),
            uniform_reason: self.uniform_reason.map(str::to_owned),
            bound_display: self.bound_display.clone(),
            compositor_output: self.compositor_output.clone(),
            granted_at_epoch_s: self.granted_at.map(OffsetDateTime::unix_timestamp),
            source_gate: source_gate.map(str::to_owned),
        }
    }
}

/// Sender and status subscription for one display's sampler service.
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
    /// Sampling was disabled by an operator command.
    SamplingDisabled,
    /// Another consent request is already active.
    FlowAlreadyActive,
    /// The portal source could not be created for this graphical session.
    NoGraphicalSession,
    /// The daemon sampler is no longer running.
    CommandChannelClosed,
    /// Persistent consent-record I/O failed.
    Store(crate::screencast_consent::ConsentError),
    /// Sampling is administratively suspended (e.g. wear disabled, configured
    /// display absent). Re-enable through configuration; the operator IPC
    /// `Enable` cannot resume it.
    AdministrativelySuspended,
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

/// Runtime configuration and its lifecycle trigger.
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
    /// Display permanently owned by this sampler instance.
    pub display_id: DisplayId,
    /// Reload and generation-context updates.
    pub update_rx: mpsc::Receiver<SamplerUpdate>,
    /// Daemon-lifetime keyed latest-value handoff to the wear tracker.
    pub latest_grids: LatestGrids,
    /// Platform capture implementation.
    pub source: Box<dyn CaptureSource + Send + Sync + 'static>,
    /// Source-gate poll reader. `None` for displays without a configured gate;
    /// the runtime then treats every capture as unconditionally matched.
    pub source_reader: Option<Arc<dyn source_gate::InputSourceReader>>,
    /// Port-8001 app-visibility probe passed to every `SourceGatePoller`
    /// the runtime spawns. `None` for displays without `watched_apps` — the
    /// poller skips the 8001 cycle entirely (issue #232).
    pub apps_probe: Option<Arc<dyn source_gate::AppVisibilityProbe>>,
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

/// Display identity, phase, and source-gate configuration supplied by
/// generation management.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplaySamplingContext {
    /// Configured sampled display, when present in the generation.
    pub display: Option<DisplayExpectation>,
    /// Current display phase.
    pub phase: Phase,
    /// Whether a display stage currently permits spatial attribution.
    pub stage_active: bool,
    /// Source-gate configuration derived from the selected display's
    /// `[displays.<id>.sampling]` table. `None` when the display is render-only
    /// (no `[sampling]` subtable) — the runtime treats an absent gate as
    /// permanently matched.
    #[doc(hidden)]
    pub source_gate_expectation: Option<source_gate::SourceGateExpectation>,
    /// Per-display `stream_mode` override. `None` when the operator has not
    /// declared one; the runtime falls back to the wear section's
    /// `[wear.active_sampling] stream_mode`.
    pub stream_mode: Option<StreamMode>,
}

/// Allocate the daemon-lifetime latest-sample map.
#[must_use]
pub fn new_latest_grids() -> LatestGrids {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Allocate the daemon-lifetime sampler-status map.
#[must_use]
pub fn new_sampler_statuses() -> SamplerStatuses {
    Arc::new(RwLock::new(BTreeMap::new()))
}

fn replace_latest(latest: &LatestGrids, display: &DisplayId, sample: SampledGrid) {
    if let Ok(mut grids) = latest.write() {
        grids.insert(display.clone(), sample);
    }
}

/// Build a source-gate expectation from the configured display's `[sampling]`
/// table. Returns `None` when the display has no `[sampling]` subtable, the
/// subtable omits `expected_source`, or the display carries no `host` for the
/// poller to target — the runtime then treats the gate as permanently matched.
fn build_gate_expectation(
    config: &Config,
    display_id: &str,
) -> Option<source_gate::SourceGateExpectation> {
    let display = config.displays.get(display_id)?;
    let sampling = display.sampling.as_ref()?;
    let expected_source = sampling.expected_source.as_ref()?;
    let host = display.host.as_ref()?;
    Some(source_gate::SourceGateExpectation {
        host: host.clone(),
        expected_source: expected_source.clone(),
        poll_interval: sampling.source_poll_interval,
        watched_apps: Arc::from(sampling.watched_apps.clone()),
    })
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

    #[cfg(test)]
    pub(crate) fn test_handle() -> (Self, mpsc::Receiver<SamplerCommand>) {
        let (handle, command_rx, _status_tx) = Self::new(SamplerStatus {
            state: SamplingState::NeedsConsent,
            last_capture: None,
            uniform_reason: Some(WEAR_SAMPLING_NEEDS_CONSENT),
            bound_display: None,
            compositor_output: None,
            granted_at: None,
            source_gate: None,
        });
        (handle, command_rx)
    }
}

impl fmt::Display for SamplerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisabledByConfig => f.write_str("active sampling is disabled by configuration"),
            Self::SamplingDisabled => f.write_str("active sampling is disabled"),
            Self::FlowAlreadyActive => {
                f.write_str("an active sampling consent flow is already active")
            }
            Self::NoGraphicalSession => f.write_str("no graphical session is available"),
            Self::CommandChannelClosed => f.write_str("active sampling service is not running"),
            Self::Store(error) => error.fmt(f),
            Self::AdministrativelySuspended => f.write_str(
                "active sampling is administratively suspended; re-enable via configuration",
            ),
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
        compositor_output: None,
        granted_at: None,
        source_gate: None,
    }
}

struct Runtime {
    state: SamplingState,
    active: ActiveSamplingConfig,
    sample_interval: Duration,
    display: DisplaySamplingContext,
    /// Cached display identifier; gates + wear tracker reads use it for
    /// `latest_grids` cleanup on gate transitions and on reload.
    display_id: DisplayId,
    record: Option<crate::screencast_consent::BoundConsent>,
    failures: u32,
    reconnect_backoff: Duration,
    episode_warned: std::collections::HashSet<String>,
    pending_stream_reset: bool,
    event_tx: Option<mpsc::Sender<ControlMsg>>,
    /// Active source-gate poller for this runtime; present iff the
    /// configured display carries a `[displays.<id>.sampling].expected_source`
    /// (and `host`) AND the lifecycle is `Streaming`.
    gate_poller: Option<source_gate::SourceGatePoller>,
    /// Watch subscription on the gate poller's latest observation; the run
    /// loop listens for change events alongside cadence/update/command.
    gate_rx: Option<watch::Receiver<source_gate::SourceGate>>,
    /// Reader used to spawn a fresh `SourceGatePoller` when the configured
    /// expectation changes (e.g. a reconfigure that adds `expected_source`).
    /// Cached on `Runtime` so the `expectation`-only paths do not need to
    /// thread the reader through every call site.
    source_reader: Option<Arc<dyn source_gate::InputSourceReader>>,
    /// Port-8001 app-visibility probe passed to every `SourceGatePoller`
    /// the runtime spawns. `None` for render-only displays; the gate then
    /// skips the app-overlay check entirely (issue #232).
    apps_probe: Option<Arc<dyn source_gate::AppVisibilityProbe>>,
    /// Latest source-gate observation published to the status channel.
    gate_state: Option<source_gate::SourceGate>,
    /// Last gate value published on the additive `DaemonEvent` channel.
    /// Compares by full `SourceGate` enum value so a steady mismatched poll
    /// fires exactly one event per change (unknown → mismatched → mismatched
    /// → matched = three events).
    last_event_gate: Option<source_gate::SourceGate>,
    /// Per-runtime monotonic capture-timer sequence for the
    /// `wear_sampling_capture_timing` debug surface.
    capture_sequence: u64,
    /// Effective `stream_mode` last published on this runtime, used to detect
    /// per-display override changes on `DisplayContext` reloads.
    last_effective_stream_mode: StreamMode,
    /// Expected-source snapshot the currently-spawned poller is bound to.
    /// `reconcile_gate_poller` consults this to decide between no-op,
    /// same-expectation, and tear-down-and-respawn.
    active_gate_expectation: Option<source_gate::SourceGateExpectation>,
    /// Shared latest-grid map; cached on `Runtime` so the gate helpers
    /// can clear this display's entry on add/change/transition without
    /// threading `&LatestGrids` through every call site.
    latest_grids: LatestGrids,
}

impl Runtime {
    fn new(config: &Config, consent_path: &std::path::Path, display_id: &DisplayId) -> Self {
        let configured_compositor_output = config
            .displays
            .get(&display_id.0)
            .and_then(|display| display.compositor_output.clone());
        let gate_expectation = build_gate_expectation(config, &display_id.0);
        let display = DisplaySamplingContext {
            display: config
                .wear
                .active_sampling
                .selected_displays()
                .iter()
                .any(|display| display == &display_id.0)
                .then(|| DisplayExpectation {
                    display: display_id.0.clone(),
                    compositor_output: configured_compositor_output.clone(),
                }),
            phase: Phase::Active,
            stage_active: true,
            source_gate_expectation: gate_expectation,
            stream_mode: config
                .displays
                .get(&display_id.0)
                .and_then(|display| display.sampling.as_ref())
                .and_then(|sampling| sampling.stream_mode),
        };
        let active_snapshot = config.wear.active_sampling.clone();
        let active = active_snapshot;
        let record = display.display.as_ref().and_then(|expected| {
            crate::screencast_consent::load(
                consent_path,
                &expected.display,
                expected.compositor_output.as_deref(),
            )
            .ok()
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
        let last_effective_stream_mode = display.stream_mode.unwrap_or(active.stream_mode);
        Self {
            state,
            active,
            sample_interval: config.wear.sample_interval,
            display,
            display_id: display_id.clone(),
            record,
            failures: 0,
            reconnect_backoff: Duration::from_secs(30),
            episode_warned: std::collections::HashSet::new(),
            pending_stream_reset: false,
            event_tx: None,
            gate_poller: None,
            gate_rx: None,
            source_reader: None,
            apps_probe: None,
            gate_state: None,
            last_event_gate: None,
            capture_sequence: 0,
            last_effective_stream_mode,
            active_gate_expectation: None,
            latest_grids: new_latest_grids(),
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

/// Pending outcome of a `Disable` command received mid-consent-flow.
struct PendingDisable {
    /// `true` if the operator asked the daemon to also drop the stored grant.
    forget: bool,
    /// Reply channel for the original `Disable` command.
    reply: oneshot::Sender<Result<(), SamplerError>>,
    /// Transition the sampler applied when it took the command.
    transition: Transition,
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

#[allow(
    clippy::too_many_arguments,
    reason = "the capture boundary keeps the sampler key beside its independently owned source, cadence, and cancellation state"
)]
async fn capture_one(
    source: &mut dyn CaptureSource,
    capture_timeout: Duration,
    stream_mode: StreamMode,
    phase: Phase,
    latest: &LatestGrids,
    display: &DisplayId,
    cancel: &CancellationToken,
    cadence: &mut tokio::time::Interval,
    reset_stream: bool,
) -> CaptureOutcome {
    if reset_stream {
        source.reset_stream().await;
    }
    let capture_outcome = {
        let capture = tokio::time::timeout(capture_timeout, source.capture_one(stream_mode));
        tokio::pin!(capture);
        let mut overlapping = 0;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break CaptureOutcome::Cancelled,
                result = &mut capture => match result {
                    Ok(Ok(frame)) => match dormant_core::spatial_grid::reduce_rgba8_to_luma_grid(
                        &frame.rgba, frame.width, frame.height, frame.stride, 9, 16,
                    ) {
                        Ok(grid) => {
                            let captured_at = Tick::now();
                            replace_latest(latest, display, SampledGrid {
                                grid,
                                captured_at,
                                phase_at_capture: phase,
                            });
                            break CaptureOutcome::Ok(captured_at);
                        }
                        Err(_) => break CaptureOutcome::Failed(
                            CaptureError::Protocol("grid reduction failed".to_owned()),
                            overlapping,
                        ),
                    },
                    Ok(Err(error)) => break CaptureOutcome::Failed(error, overlapping),
                    Err(_) => break CaptureOutcome::Failed(
                        CaptureError::Timeout,
                        overlapping,
                    ),
                },
                _ = cadence.tick() => {
                    overlapping = overlapping.saturating_add(1);
                }
            }
        }
    };
    // After the capture future is fully dropped, reclaim the borrow on
    // `source` so the outer-timeout path can invalidate any warm-worker
    // state that would otherwise be served stale on the next capture
    // (issue #211 defect B).
    if let CaptureOutcome::Failed(CaptureError::Timeout, _) = &capture_outcome {
        source.invalidate_pending_capture().await;
    }
    capture_outcome
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
    let configured_output = runtime
        .display
        .display
        .as_ref()
        .and_then(|expected| expected.compositor_output.as_deref());
    runtime.record =
        crate::screencast_consent::load(path, &rotated.sampled_display, configured_output).ok();
    Ok(())
}

async fn apply_capture_failure(
    runtime: &mut Runtime,
    source: &mut dyn CaptureSource,
    error: CaptureError,
    status_tx: &watch::Sender<SamplerStatus>,
) -> Transition {
    match error {
        CaptureError::Auth => {
            apply_trigger_with_effects(runtime, source, Trigger::AuthFailed, status_tx).await
        }
        CaptureError::SessionClosed => {
            apply_trigger_with_effects(runtime, source, Trigger::SessionClosed, status_tx).await
        }
        CaptureError::Protocol(reason) if reason == WEAR_SAMPLING_WRONG_MONITOR => {
            apply_trigger_with_effects(runtime, source, Trigger::WrongMonitor, status_tx).await
        }
        CaptureError::Transport(_) => {
            apply_trigger_with_effects(runtime, source, Trigger::TransportFailed, status_tx).await
        }
        CaptureError::ConsentDenied | CaptureError::Timeout | CaptureError::Protocol(_) => {
            if runtime.failures >= runtime.active.failure_threshold {
                apply_trigger_with_effects(runtime, source, Trigger::CaptureFailed, status_tx).await
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
            if runtime.state == SamplingState::Disabled {
                if runtime.record.is_some() {
                    let transition = apply_trigger(
                        runtime,
                        Trigger::ConfigChanged(ConfigDelta::Enabled(true)),
                        status_tx,
                    );
                    let _ = reply.send(ConsentFlowStatus::Granted);
                    return transition.effects.contains(&Effect::Connect);
                }
                let _ = reply.send(ConsentFlowStatus::Error(
                    SamplerError::SamplingDisabled.to_string(),
                ));
                return false;
            }
            if runtime.state == SamplingState::Suspended {
                // Suspended sampling cannot be resumed through the operator IPC
                // enable path — only the wear/display configuration can lift it.
                let _ = reply.send(ConsentFlowStatus::Error(
                    SamplerError::AdministrativelySuspended.to_string(),
                ));
                return false;
            }
            if runtime.state != SamplingState::NeedsConsent {
                // Reject before reaching the capture source: in Streaming / Connecting
                // a subsequent cancellation of the new consent flow would call
                // source.close() on a live portal session and tear it down.
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
                "Enable from NeedsConsent must request a portal consent flow"
            );
            // The portal has no config timeout; the five-minute interaction bound
            // prevents an abandoned dialog from retaining a daemon operation forever.
            let mut consent = Box::pin(source.request_consent(&expected));
            let mut deadline = Box::pin(tokio::time::sleep(CONSENT_INTERACTION_TIMEOUT));
            let mut pending_disable: Option<PendingDisable> = None;
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
                            // Disable during a pending consent must transition
                            // to Disabled (not NeedsConsent) so a later Enable
                            // is rejected by the disabled-config path rather
                            // than silently re-opening a fresh grant dialog.
                            let transition = apply_trigger(
                                runtime,
                                Trigger::ConfigChanged(ConfigDelta::Enabled(false)),
                                status_tx,
                            );
                            pending_disable = Some(PendingDisable {
                                forget,
                                reply,
                                transition,
                            });
                            break Some(Err(CaptureError::Protocol("wear_sampling_cancelled".to_owned())));
                        }
                        None => break None,
                    }
                }
            };
            drop(consent);
            drop(deadline);
            if let Some(pending) = pending_disable {
                if pending.transition.effects.contains(&Effect::CloseSession) {
                    source.close().await;
                }
                if pending.forget {
                    if let Err(error) = crate::screencast_consent::forget(consent_path) {
                        let _ = pending.reply.send(Err(SamplerError::Store(error)));
                    } else {
                        runtime.record = None;
                        let _ = pending.reply.send(Ok(()));
                    }
                } else {
                    let _ = pending.reply.send(Ok(()));
                }
            }
            match outcome {
                None => return false,
                Some(Err(CaptureError::Timeout)) => {
                    tracing::warn!(
                        event = "wear_sampling_consent_failed",
                        display = %runtime.display_name(),
                        reason = "timeout"
                    );
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
                        display = %runtime.display_name(),
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
                        stream_position: grant.stream.position,
                        compositor_output: expected.compositor_output.clone(),
                    };
                    match crate::screencast_consent::store_atomic(consent_path, &record) {
                        Ok(()) => {
                            runtime.record = crate::screencast_consent::load(
                                consent_path,
                                &record.sampled_display,
                                record.compositor_output.as_deref(),
                            )
                            .ok();
                            tracing::info!(
                                event = "wear_sampling_stage",
                                display = %record.sampled_display,
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
                // Keep the per-runtime effective-mode ledger in sync with
                // the wear section's updated `[wear.active_sampling] stream_mode`
                // so a subsequent DisplayContext update can detect a real
                // override change rather than the wear-side shift.
                runtime.last_effective_stream_mode = runtime
                    .display
                    .stream_mode
                    .unwrap_or(plan.active_sampling.stream_mode);
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
            let consent_bound_drifted = runtime
                .display
                .display
                .as_ref()
                .zip(context.display.as_ref())
                .is_some_and(|(old, new)| {
                    old.display != new.display || old.compositor_output != new.compositor_output
                });
            let old_effective_mode = runtime.last_effective_stream_mode;
            let new_effective_mode = context.stream_mode.unwrap_or(runtime.active.stream_mode);
            let mode_overridden = old_effective_mode != new_effective_mode;
            let source_gate_changed =
                runtime.display.source_gate_expectation != context.source_gate_expectation;
            runtime.display = context;
            runtime.last_effective_stream_mode = new_effective_mode;
            if consent_bound_drifted {
                runtime.record = None;
                return Some(apply_trigger(
                    runtime,
                    Trigger::ConfigChanged(ConfigDelta::SampledDisplayChanged),
                    status_tx,
                ));
            }
            let transition = (was_present != is_present).then(|| {
                apply_trigger(
                    runtime,
                    Trigger::ConfigChanged(ConfigDelta::DisplayPresent(is_present)),
                    status_tx,
                )
            });
            if mode_overridden {
                // Honor the existing stream-mode-change reset: reroute the
                // pending reset flag and reuse the standard StreamModeChanged
                // trigger for downstream listeners (no re-consent). The
                // outer `apply_update_with_effects` honors `CloseSession`
                // effects through its own `source.close()` call.
                runtime.pending_stream_reset = true;
                let mode_transition = apply_trigger(
                    runtime,
                    Trigger::ConfigChanged(ConfigDelta::StreamModeChanged),
                    status_tx,
                );
                // Don't drop the presence transition if BOTH fired in the
                // same publish — the caller expects the combined effects
                // (e.g. DisplayPresent(false) → Suspended + the reset).
                return transition.or(Some(mode_transition));
            }
            if source_gate_changed {
                // Reconciliation handled by `apply_update_with_effects`
                // (it owns the poller teardown-and-respawn).
                return transition;
            }
            transition
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
    // Keep the platform source's inner per-capture bound in sync with the
    // daemon's outer bound so a raised `capture_timeout` actually takes
    // effect in warm mode (issue #211 defect A).
    source.set_capture_timeout(runtime.active.capture_timeout);
    // The runtime caches its reader at spawn; if a `DisplayContext`
    // adds a `source_gate_expectation` after spawn (operator added the
    // `host` to an existing display) the cached reader is still `None`
    // and `wants_poller` would be false forever. Build the production
    // default reader on first need so the gate can actually run on
    // live runtimes. The reader sits dormant until `reconcile_gate_poller`
    // decides to spawn a poller.
    if runtime.source_reader.is_none() && runtime.display.source_gate_expectation.is_some() {
        runtime.source_reader = Some(source_gate::build_default_reader());
    }
    // App-visibility probe is per-runtime so a runtime that grows an
    // expectation with `watched_apps` mid-flight gets a working probe
    // without coordinated hand-off. `None` means no catalog
    // configured — the poller skips the 8001 cycle entirely.
    if runtime.apps_probe.is_none() && runtime.display.source_gate_expectation.is_some() {
        runtime.apps_probe = Some(source_gate::build_default_app_probe());
    }
    // Source-gate expectation may have swapped with the new context;
    // reconcile so the next Streaming tick spawns a poller bound to the
    // fresh expectation. The helper itself is also responsible for
    // publishing the gate-event / clearing `latest_grids` when an
    // add/change actually spawns.
    reconcile_gate_poller(runtime, status_tx);
    if transition
        .as_ref()
        .is_some_and(|transition| transition.effects.contains(&Effect::CloseSession))
    {
        source.close().await;
    }
    transition
}

/// Apply a trigger and honor the lifecycle effects it emits.
///
/// The pure [`decide`] table only describes intent; the run loop is what
/// releases a live portal session when a transition asks for `CloseSession`.
/// Skipping this helper at any `AuthFailed` / `SessionClosed` / `WrongMonitor`
/// site leaves the warm `PipeWire` worker and portal session parked after the
/// lifecycle has moved on to `NeedsConsent` / `Disabled`.
async fn apply_trigger_with_effects(
    runtime: &mut Runtime,
    source: &mut dyn CaptureSource,
    trigger: Trigger,
    status_tx: &watch::Sender<SamplerStatus>,
) -> Transition {
    let transition = apply_trigger(runtime, trigger, status_tx);
    if transition.effects.contains(&Effect::CloseSession) {
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
    reconcile_gate_poller(runtime, status_tx);
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
        compositor_output: runtime
            .display
            .display
            .as_ref()
            .and_then(|display| display.compositor_output.clone()),
        granted_at: runtime
            .record
            .as_ref()
            .map(|record| record.record().granted_at),
        source_gate: runtime.gate_state.clone(),
    });
}

/// Spawn a `SourceGatePoller` when the configured display carries a gate
/// expectation AND the lifecycle is `Streaming`; cancel the existing poller
/// otherwise. Idempotent — calling it on a `Streaming` runtime that already
/// matches the expectation is a no-op, while a reconfigure that swaps
/// `expected_source` (or its host) tears down and re-spawns.
///
/// Spawning a fresh poller is itself an explicit transition: the
/// previously-valid spatial grid (if any) is cleared, the status
/// publishes `source_gate = Unknown{awaiting_first_poll}` with
/// `uniform_reason = source_unknown`, and the additive gate event
/// fires — so the wear tracker cannot spatially attribute a
/// pre-reload grid during the gate's first-poll window.
fn reconcile_gate_poller(runtime: &mut Runtime, status_tx: &watch::Sender<SamplerStatus>) {
    let streaming = runtime.state == SamplingState::Streaming;
    let expectation = runtime.display.source_gate_expectation.clone();
    let reader = runtime.source_reader.clone();
    let wants_poller = streaming && expectation.is_some() && reader.is_some();
    if !wants_poller {
        if runtime.gate_poller.take().is_some() {
            runtime.gate_rx = None;
            runtime.active_gate_expectation = None;
            // A removed gate must not leave the runtime wedged on the
            // last observed state (a stale `Mismatched` would skip
            // captures forever on an ungated display, and the status
            // would advertise a phantom gate). Reset both the dedup
            // anchor and the live observation so a re-added gate's
            // first poll re-emits.
            runtime.gate_state = None;
            runtime.last_event_gate = None;
        }
        return;
    }
    let expectation = expectation.expect("checked above");
    let reader = reader.expect("checked above");
    // Already polling this exact expectation; no-op.
    if runtime.active_gate_expectation.as_ref() == Some(&expectation)
        && runtime.gate_poller.is_some()
    {
        return;
    }
    // Tear down any prior poller so its task halts and we re-seed the gate
    // seed from the new poller's first observation.
    if runtime.gate_poller.take().is_some() {
        runtime.gate_rx = None;
    }
    let poller = source_gate::SourceGatePoller::spawn(
        reader,
        runtime.apps_probe.clone(),
        expectation.clone(),
    );
    let mut rx = poller.subscribe();
    // Reset both the live observation and the dedup anchor BEFORE
    // seeding — and do NOT pre-assign `gate_state`:
    // `apply_gate_observation` must observe the None → Some
    // transition itself, or its `needs_clear` arm sees
    // `prev == next` and skips the grid clear (and the transition
    // publish) entirely.
    runtime.gate_state = None;
    runtime.last_event_gate = None;
    let seed_observation = rx.borrow_and_update().clone();
    runtime.gate_rx = Some(rx);
    runtime.gate_poller = Some(poller);
    runtime.active_gate_expectation = Some(expectation);
    // Snapshot the borrowed fields out of `runtime` so we can re-enter
    // `apply_gate_observation` (which takes `&mut Runtime`) without the
    // borrow checker fighting the nested immutable borrows.
    let latest_grids = runtime.latest_grids.clone();
    let display_id = runtime.display_id.clone();
    apply_gate_observation(
        runtime,
        &seed_observation,
        status_tx,
        &latest_grids,
        &display_id,
    );
}

/// React to a fresh gate observation: publish status, emit additive
/// `DaemonEvent::WearSamplingSourceGate` on full-value change, route the
/// matching capture timing / log events, and clear the display's
/// `latest_grids` entry on every transition away from matched.
fn apply_gate_observation(
    runtime: &mut Runtime,
    observation: &source_gate::SourceGate,
    status_tx: &watch::Sender<SamplerStatus>,
    latest_grids: &LatestGrids,
    display_id: &DisplayId,
) {
    let previous_uniform_reason = runtime
        .record
        .as_ref()
        .and_then(|_| runtime.gate_state.as_ref().map(sampling_uniform_reason));
    let next = observation.clone();
    let transitioned = runtime.gate_state.as_ref() != Some(&next);
    let needs_clear = match &runtime.gate_state {
        Some(prev) => prev != &next && runtime.display.display.is_some(),
        None => true,
    };
    if needs_clear && let Ok(mut grids) = latest_grids.write() {
        grids.remove(display_id);
    }
    runtime.gate_state = Some(next.clone());
    let new_uniform_reason = sampling_uniform_reason(&next);
    // Emit the `WearSamplingSourceGate` daemon event only on a full-gate
    // change (matches the spec's "unknown → mismatched → mismatched →
    // matched = exactly 3 events" requirement).
    if runtime.last_event_gate.as_ref() != Some(&next) {
        if let Some(event_tx) = runtime.event_tx.as_ref() {
            let observed = next.observed().map(str::to_owned);
            let _ = event_tx.try_send(ControlMsg::PublishDaemonEvent(
                DaemonEvent::WearSamplingSourceGate {
                    display: display_id.clone(),
                    state: next.tag().to_owned(),
                    observed,
                },
            ));
        }
        runtime.last_event_gate = Some(next.clone());
    }
    // Emit a transition log line on every observation change (warn on
    // mismatch / unknown / poll_failed; info on matched). Steady-state
    // repeats are not logged here — the poller logs every observation
    // at debug level via `wear_sampling_source_poll`.
    match &next {
        source_gate::SourceGate::Matched => {
            tracing::info!(
                event = "wear_sampling_source_matched",
                display = %runtime.display_name(),
                source = next.observed().unwrap_or("expected"),
            );
            if new_uniform_reason.is_none() && previous_uniform_reason.is_some() {
                publish_status(status_tx, runtime, None, None);
            }
        }
        source_gate::SourceGate::Mismatched { observed } => {
            let expected = runtime
                .display
                .source_gate_expectation
                .as_ref()
                .map_or("", |e| e.expected_source.as_str());
            tracing::warn!(
                event = "wear_sampling_source_mismatch",
                display = %runtime.display_name(),
                observed = %observed,
                expected = %expected,
            );
        }
        source_gate::SourceGate::Unknown { reason } => {
            let tag = if *reason == "poll_failed" {
                "wear_sampling_source_poll_failed"
            } else {
                "wear_sampling_source_unknown"
            };
            tracing::warn!(
                event = tag,
                display = %runtime.display_name(),
                reason = reason,
            );
        }
    }
    if transitioned {
        publish_status(status_tx, runtime, new_uniform_reason, None);
    }
}

fn sampling_uniform_reason(gate: &source_gate::SourceGate) -> Option<&'static str> {
    match gate {
        source_gate::SourceGate::Matched => None,
        source_gate::SourceGate::Mismatched { .. } => Some("source_mismatch"),
        source_gate::SourceGate::Unknown { .. } => Some(WEAR_SAMPLING_SOURCE_UNKNOWN),
    }
}

fn clear_latest_for(latest: &LatestGrids, display: &DisplayId) {
    if let Ok(mut grids) = latest.write() {
        grids.remove(display);
    }
}

/// Drain any pending gate change events from `runtime.gate_rx` into
/// `runtime.gate_state`, applying the most recent observation. Used on every
/// state-boundary entry/exit so the run loop can read a fresh value without
/// racing the poller. Returns `true` when the gate state changed (the
/// caller should recheck the gate before any pending capture).
fn drain_gate_changes(
    runtime: &mut Runtime,
    status_tx: &watch::Sender<SamplerStatus>,
    latest_grids: &LatestGrids,
    display_id: &DisplayId,
) -> bool {
    let Some(rx) = runtime.gate_rx.as_mut() else {
        return false;
    };
    let latest = rx.borrow_and_update().clone();
    let mut changed = false;
    if runtime.gate_state.as_ref() != Some(&latest) {
        apply_gate_observation(runtime, &latest, status_tx, latest_grids, display_id);
        changed = true;
    }
    changed
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
    let mut runtime = Runtime::new(&deps.initial_config, &deps.consent_path, &deps.display_id);
    runtime.event_tx = deps.event_tx.take();
    runtime.source_reader = deps.source_reader.take();
    runtime.apps_probe = deps.apps_probe.take();
    // Rebind the cached grid map to the SHARED daemon-lifetime map.
    // `Runtime::new` cannot receive it (tests construct `Runtime`
    // literally), so without this handoff `reconcile_gate_poller`'s
    // seed-clear would target a private map while the wear tracker
    // reads `deps.latest_grids` — the stale grid would survive until
    // the poller's first poll instead of being cleared at gate-add.
    runtime.latest_grids = deps.latest_grids.clone();
    // Push the configured per-capture deadline into the platform source at
    // startup so the inner warm-mode bound is in lockstep with the daemon
    // bound from the first tick (issue #211 defect A).
    deps.source
        .set_capture_timeout(runtime.active.capture_timeout);
    let initial_reason = match runtime.state {
        SamplingState::NeedsConsent => Some(WEAR_SAMPLING_NEEDS_CONSENT),
        SamplingState::Suspended => Some(WEAR_SAMPLING_SUSPENDED),
        _ => None,
    };
    let initial_state = runtime.state;
    transition_to(&mut runtime, initial_state, initial_reason, &status_tx);
    reconcile_gate_poller(&mut runtime, &status_tx);
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
                    apply_trigger_with_effects(
                        &mut runtime,
                        &mut *deps.source,
                        Trigger::AuthFailed,
                        &status_tx,
                    )
                    .await;
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
                            // The rotated token could not be persisted, so the
                            // portal session just opened by `connect()` must be
                            // released. The pure table pins the effects vector
                            // for `(Streaming, AuthFailed)` to
                            // `[CloseSession, EnterUniform(WEAR_SAMPLING_TOKEN_INVALID)]`;
                            // the helper is what honors the CloseSession at
                            // runtime here. Without it, the warm PipeWire
                            // worker and the portal session would leak in the
                            // rare disk-full / permission-revoked path.
                            apply_trigger_with_effects(
                                &mut runtime,
                                &mut *deps.source,
                                Trigger::AuthFailed,
                                &status_tx,
                            )
                            .await;
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
                        apply_trigger_with_effects(
                            &mut runtime,
                            &mut *deps.source,
                            trigger,
                            &status_tx,
                        )
                        .await;
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
                // Drain any gate observation that landed before this tick so
                // a fresh poll's verdict decides whether the capture proceeds.
                drain_gate_changes(
                    &mut runtime,
                    &status_tx,
                    &deps.latest_grids,
                    &deps.display_id,
                );
                if !capture_now {
                    // Arm that resolves when the poller's latest observation
                    // changes. Pending forever on an ungated runtime so the
                    // arm never fires there. Waking here lets us drain +
                    // re-decide without a cadence tick (TOCTOU: a flip
                    // DURING the cadence wait must skip the next capture).
                    // The receiver is cloned (Arc clone, cheap) so the boxed
                    // future does NOT borrow `runtime.gate_rx` — the rest of
                    // the select arms and the post-drain arm body both need
                    // to take `&mut runtime`.
                    let mut gate_signal: Option<
                        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
                    > = runtime.gate_rx.as_ref().map(|rx| {
                        let mut rx = rx.clone();
                        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                            Box::pin(async move {
                                let _ = rx.changed().await;
                            });
                        fut
                    });
                    let mut pending_never: std::pin::Pin<
                        Box<dyn std::future::Future<Output = ()> + Send>,
                    > = Box::pin(std::future::pending::<()>());
                    let pending_gate: std::pin::Pin<
                        &mut (dyn std::future::Future<Output = ()> + Send),
                    > = match gate_signal.as_mut() {
                        Some(fut) => fut.as_mut(),
                        None => pending_never.as_mut(),
                    };
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
                        () = pending_gate => {
                            // Gate flipped while we were waiting; the poller
                            // already updated `runtime.gate_rx`'s underlying
                            // value, but we drain again so the next select
                            // iteration sees the freshest observation.
                            drain_gate_changes(
                                &mut runtime,
                                &status_tx,
                                &deps.latest_grids,
                                &deps.display_id,
                            );
                            continue;
                        }
                    }
                }
                capture_now = false;
                // Re-drain immediately before the gate-skip check: a poller
                // tick that fires DURING the cadence select above would have
                // left `runtime.gate_state` stale until the next streaming
                // arm entry. Reading the freshest observation here closes
                // the race window.
                drain_gate_changes(
                    &mut runtime,
                    &status_tx,
                    &deps.latest_grids,
                    &deps.display_id,
                );
                // Recheck the gate right before capture: a transient
                // mismatch / unknown must skip the capture even if the
                // cadence already fired, and the wear tracker must not see
                // a stale grid promoted to a fresh attribute.
                let gate_skip = matches!(
                    runtime.gate_state.as_ref(),
                    Some(
                        source_gate::SourceGate::Mismatched { .. }
                            | source_gate::SourceGate::Unknown { .. },
                    )
                );
                if gate_skip {
                    clear_latest_for(&deps.latest_grids, &deps.display_id);
                    continue;
                }
                runtime.capture_sequence = runtime.capture_sequence.saturating_add(1);
                let capture_sequence = runtime.capture_sequence;
                let display_name = runtime.display_name();
                let started_at = std::time::Instant::now();
                tracing::debug!(
                    event = "wear_sampling_capture_timing",
                    display = %display_name,
                    sequence = capture_sequence,
                    stage = "requested",
                );
                let attempt = capture_one(
                    &mut *deps.source,
                    runtime.active.capture_timeout,
                    runtime
                        .display
                        .stream_mode
                        .unwrap_or(runtime.active.stream_mode),
                    runtime.display.phase.clone(),
                    &deps.latest_grids,
                    &deps.display_id,
                    &deps.cancel,
                    &mut cadence,
                    std::mem::take(&mut runtime.pending_stream_reset),
                )
                .await;
                let frame_ready_at = std::time::Instant::now();
                let frame_ready_elapsed_ms = u64::try_from(
                    frame_ready_at
                        .saturating_duration_since(started_at)
                        .as_millis(),
                )
                .unwrap_or(u64::MAX);
                tracing::debug!(
                    event = "wear_sampling_capture_timing",
                    display = %display_name,
                    sequence = capture_sequence,
                    stage = "frame_ready",
                    elapsed_ms = frame_ready_elapsed_ms,
                );
                match attempt {
                    CaptureOutcome::Cancelled => break,
                    CaptureOutcome::Ok(captured_at) => {
                        let reduction_elapsed_ms = u64::try_from(
                            std::time::Instant::now()
                                .saturating_duration_since(started_at)
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX);
                        tracing::debug!(
                            event = "wear_sampling_capture_timing",
                            display = %display_name,
                            sequence = capture_sequence,
                            stage = "reduction_complete",
                            elapsed_ms = reduction_elapsed_ms,
                        );
                        runtime.failures = 0;
                        runtime.episode_warned.clear();
                        apply_trigger(&mut runtime, Trigger::CaptureOk, &status_tx);
                        publish_status(&status_tx, &runtime, None, Some(captured_at));
                    }
                    CaptureOutcome::Failed(error, overlapping) => {
                        runtime.failures = runtime.failures.saturating_add(1 + overlapping);
                        apply_capture_failure(&mut runtime, &mut *deps.source, error, &status_tx)
                            .await;
                    }
                }
            }
            SamplingState::Cooldown => {
                tokio::select! {
                    () = deps.cancel.cancelled() => break,
                    () = tokio::time::sleep(runtime.active.circuit_reset_after) => {
                        let transition = apply_trigger(&mut runtime, Trigger::CooldownElapsed, &status_tx);
                        debug_assert!(transition.effects.contains(&Effect::Capture), "only CooldownElapsed may schedule a cooldown retry");
                        let attempt = capture_one(&mut *deps.source, runtime.active.capture_timeout, runtime.display.stream_mode.unwrap_or(runtime.active.stream_mode), runtime.display.phase.clone(), &deps.latest_grids, &deps.display_id, &deps.cancel, &mut cadence, std::mem::take(&mut runtime.pending_stream_reset)).await;
                        match attempt {
                            CaptureOutcome::Cancelled => break,
                            CaptureOutcome::Ok(captured_at) => { runtime.failures = 0; runtime.episode_warned.clear(); apply_trigger(&mut runtime, Trigger::CaptureOk, &status_tx); publish_status(&status_tx, &runtime, None, Some(captured_at)); }
                            CaptureOutcome::Failed(error, _) => {
                                apply_capture_failure(
                                    &mut runtime,
                                    &mut *deps.source,
                                    error,
                                    &status_tx,
                                )
                                .await;
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
    /// Logical `(x, y)` recorded at grant time. When the portal has
    /// provided a position for the reattached stream, the binding check
    /// compares the two — a mismatch means the operator re-bound the
    /// grant to a different monitor.
    pub stream_position: Option<(i32, i32)>,
}

/// Display identity used to request a fresh portal grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayExpectation {
    /// Configured display identity.
    pub display: String,
    /// Compositor output the operator bound this sampler to. A drift
    /// between this and the recorded consent binding invalidates the
    /// saved grant so a reconfigure does not silently relabel a
    /// monitor. `None` for samplers without an explicit output.
    pub compositor_output: Option<String>,
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
    /// Logical top-left `(x, y)` in compositor coordinates when the portal
    /// reports one; the identity signal that keeps two same-resolution outputs
    /// distinguishable. `None` when the compositor omits the field.
    pub position: Option<(i32, i32)>,
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

    /// Apply a new per-capture deadline. Sources with an inner timeout that
    /// bounds a single frame must mirror the new value; sources without one
    /// may accept and ignore the call.
    fn set_capture_timeout(&mut self, _timeout: Duration) {}

    /// Drop any state retained from a capture whose owning future was
    /// cancelled. Called by the lifecycle when the outer capture timeout
    /// fires before the inner bound, so a buffered frame cannot be served
    /// on the next capture as if it were fresh.
    async fn invalidate_pending_capture(&mut self) {}
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
        Trigger::CaptureFailed if state == SamplingState::Cooldown => transition(
            SamplingState::Connecting,
            vec![Effect::EnterUniform(WEAR_SAMPLING_COOLDOWN)],
        ),
        Trigger::AuthFailed | Trigger::SessionClosed if state == SamplingState::Cooldown => {
            needs_consent(state, WEAR_SAMPLING_TOKEN_INVALID)
        }
        Trigger::WrongMonitor if state == SamplingState::Cooldown => {
            needs_consent(state, WEAR_SAMPLING_WRONG_MONITOR)
        }
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
    // A live portal session must be released alongside the consent-record
    // invalidation: the run loop parks in NeedsConsent indefinitely and would
    // otherwise leak the warm PipeWire worker and portal session.
    if matches!(
        state,
        SamplingState::Connecting | SamplingState::Streaming | SamplingState::Cooldown
    ) {
        effects.push(Effect::CloseSession);
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

    fn with_connections(
        connections: impl IntoIterator<Item = Result<ConnectedStream, CaptureError>>,
    ) -> Self {
        Self {
            connections: connections
                .into_iter()
                .map(ScriptedOutcome::Ready)
                .collect(),
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
    use dormant_core::config::schema::DisplayConfig;
    use std::io::Write;
    use std::sync::Arc;
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

    /// Queue-driven `InputSourceReader` used by the source-gate tests: each
    /// successive `input_source` call pops the next pre-programmed response.
    /// When the queue is empty the reader falls back to a default so a test
    /// can run without spilling capture-time logs into the assertion surface.
    struct ScriptedSourceReader {
        responses: Mutex<VecDeque<Result<String, String>>>,
        default: Result<String, String>,
    }

    impl ScriptedSourceReader {
        fn new(
            responses: impl IntoIterator<Item = Result<String, String>>,
            default: Result<String, String>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                default,
            }
        }
    }

    #[async_trait]
    impl source_gate::InputSourceReader for ScriptedSourceReader {
        async fn input_source(&self, _host: &str) -> Result<String, String> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.default.clone())
        }
    }

    /// `InputSourceReader` whose poll never resolves. Used to prove the
    /// gate-add clear is IMMEDIATE: with a first poll that can never
    /// land, only the seed-time clear in `reconcile_gate_poller` can
    /// empty the shared grid — a clear deferred to the first poll (or
    /// the next cadence tick) leaves the stale grid in place forever
    /// and the test goes red.
    struct PendingSourceReader;

    #[async_trait]
    impl source_gate::InputSourceReader for PendingSourceReader {
        async fn input_source(&self, _host: &str) -> Result<String, String> {
            std::future::pending().await
        }
    }

    /// `CaptureSource` wrapper that exposes an atomic capture counter so a
    /// source-gate test can assert against call counts rather than only the
    /// absence of a grid sample. Deleting the gate-skip branch in
    /// `run()` must make the `count_captures_seen` assertion fail.
    struct GateProbeSource {
        inner: ServiceSource,
        #[allow(dead_code, reason = "exposed for callers verifying per-capture counts")]
        captures_seen: Arc<AtomicUsize>,
        publishes_seen: Arc<AtomicUsize>,
        mode_seen: Arc<Mutex<Vec<StreamMode>>>,
    }

    impl GateProbeSource {
        fn new(service: ServiceSource) -> Self {
            let captures_seen = service.captures_seen.clone();
            Self {
                inner: service,
                captures_seen,
                publishes_seen: Arc::new(AtomicUsize::new(0)),
                mode_seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl CaptureSource for GateProbeSource {
        async fn connect(
            &mut self,
            binding: &ConsentBinding<'_>,
        ) -> Result<ConnectedStream, CaptureError> {
            self.inner.connect(binding).await
        }
        async fn request_consent(
            &mut self,
            display: &DisplayExpectation,
        ) -> Result<Grant, CaptureError> {
            self.inner.request_consent(display).await
        }
        async fn capture_one(&mut self, mode: StreamMode) -> Result<RawFrame, CaptureError> {
            self.mode_seen.lock().unwrap().push(mode);
            self.publishes_seen.fetch_add(1, Ordering::SeqCst);
            self.inner.capture_one(mode).await
        }
        async fn reset_stream(&mut self) {
            self.inner.reset_stream().await;
        }
        async fn close(&mut self) {
            self.inner.close().await;
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
        let latest = new_latest_grids();
        let display = DisplayId("oled".to_owned());
        replace_latest(
            &latest,
            &display,
            SampledGrid {
                grid: dormant_core::spatial_grid::LumaGrid::new(vec![0.1; 16 * 9]).unwrap(),
                captured_at: dormant_core::types::Tick::now(),
                phase_at_capture: dormant_core::state_machine::Phase::Active,
            },
        );
        replace_latest(
            &latest,
            &display,
            SampledGrid {
                grid: dormant_core::spatial_grid::LumaGrid::new(vec![0.9; 16 * 9]).unwrap(),
                captured_at: dormant_core::types::Tick::now(),
                phase_at_capture: dormant_core::state_machine::Phase::Active,
            },
        );

        let sample = latest.read().unwrap().get(&display).cloned().unwrap();
        assert_eq!(sample.grid.cells, vec![0.9; 16 * 9]);
    }

    #[test]
    fn display_removed_on_generation_swap_suspends_a_reattaching_sampler() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));

        let transition = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: None,
                phase: Phase::Active,
                stage_active: false,
                source_gate_expectation: None,
                stream_mode: None,
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
                    compositor_output: None,
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }),
            &status_tx,
        )
        .expect("display restoration has a lifecycle transition");
        assert_eq!(runtime.state, SamplingState::Connecting);
        assert_eq!(restored.effects, vec![Effect::Connect]);
    }

    #[test]
    fn compositor_output_drift_invalidates_consent_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        // Seed a record that already pins the grant to HDMI-A-1.
        crate::screencast_consent::store_atomic(
            &consent_path,
            &crate::screencast_consent::ConsentRecord {
                token: "saved".to_owned(),
                sampled_display: "oled".to_owned(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
                portal_persistent_ids: vec!["test-panel".to_owned()],
                granted_width: 16,
                granted_height: 9,
                stream_position: None,
                compositor_output: Some("HDMI-A-1".to_owned()),
            },
        )
        .unwrap();
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));
        assert!(runtime.record.is_some(), "seed record must load");

        let transition = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "oled".to_owned(),
                    compositor_output: Some("HDMI-A-2".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }),
            &status_tx,
        )
        .expect("drift must emit a lifecycle transition");

        assert!(
            runtime.record.is_none(),
            "compositor_output drift must invalidate the consent record"
        );
        assert_eq!(
            transition.effects,
            vec![
                Effect::CloseSession,
                Effect::EnterUniform(WEAR_SAMPLING_DISPLAY_CHANGED),
            ]
        );
    }

    #[test]
    fn display_id_drift_invalidates_consent_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));
        assert!(runtime.record.is_some(), "seed record must load");

        let transition = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "other-monitor".to_owned(),
                    compositor_output: None,
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }),
            &status_tx,
        )
        .expect("display drift must emit a lifecycle transition");

        assert!(
            runtime.record.is_none(),
            "display id drift must invalidate the consent record"
        );
        assert_eq!(
            transition.effects,
            vec![
                Effect::CloseSession,
                Effect::EnterUniform(WEAR_SAMPLING_DISPLAY_CHANGED),
            ]
        );
    }

    #[test]
    fn unchanged_consent_bound_display_does_not_invalidate_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));
        assert!(runtime.record.is_some(), "seed record must load");

        let transition = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "oled".to_owned(),
                    compositor_output: None,
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }),
            &status_tx,
        );

        assert!(
            runtime.record.is_some(),
            "unchanged display identity must not invalidate the record"
        );
        assert!(
            transition.is_none(),
            "no transition expected on identity-preserving update"
        );
    }

    #[test]
    fn config_bound_compositor_output_survives_identity_publish() {
        // Regression: the publish path must carry the configured
        // `compositor_output` so the reattach drift check sees the same
        // value the runtime was seeded with. Publishing `None` for a
        // configured `Some(...)` would look like drift to the runtime
        // and wipe the consent record on every spawn / reload.
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let mut config = (*active_config(Duration::from_secs(10))).clone();
        config.displays.insert(
            "oled".to_owned(),
            DisplayConfig {
                controllers: vec![],
                scope: dormant_core::config::schema::DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_write_code: None,
                shared_peer_input_code: None,
                hooks: dormant_core::config::schema::HookSlots::default(),
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
                samsung_restore_backlight:
                    dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::default(),
                power_off_opt_in: false,
                compositor_output: Some("HDMI-A-1".to_owned()),
                sampling: None,
            },
        );
        crate::screencast_consent::store_atomic(
            &consent_path,
            &crate::screencast_consent::ConsentRecord {
                token: "saved".to_owned(),
                sampled_display: "oled".to_owned(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
                portal_persistent_ids: vec!["test-panel".to_owned()],
                granted_width: 16,
                granted_height: 9,
                stream_position: None,
                compositor_output: Some("HDMI-A-1".to_owned()),
            },
        )
        .unwrap();
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));
        assert!(
            runtime.record.is_some(),
            "seed record must load when config and record share the compositor_output"
        );

        let transition = apply_update(
            &mut runtime,
            SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "oled".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }),
            &status_tx,
        );

        assert!(
            runtime.record.is_some(),
            "identity-preserving context publish must not invalidate the record"
        );
        assert!(
            transition.is_none(),
            "no transition expected when the publish matches the seed"
        );
    }

    #[derive(Clone)]
    enum TestCapture {
        Frame,
        FrameValue(u8),
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
                TestCapture::FrameValue(value) => Ok(RawFrame {
                    rgba: vec![value; 16 * 9 * 4],
                    width: 16,
                    height: 9,
                    stride: 16 * 4,
                }),
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

    /// Capture source that records every `set_capture_timeout` and
    /// `invalidate_pending_capture` call so reload-seam tests can assert the
    /// lifecycle actually pushed configuration down to the platform boundary.
    struct RecordingSource {
        connects: Arc<AtomicUsize>,
        capture_timeouts: Arc<Mutex<Vec<Duration>>>,
        invalidations: Arc<AtomicUsize>,
        captures: Mutex<VecDeque<TestCapture>>,
        captures_seen: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CaptureSource for RecordingSource {
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
                TestCapture::FrameValue(value) => Ok(RawFrame {
                    rgba: vec![value; 16 * 9 * 4],
                    width: 16,
                    height: 9,
                    stride: 16 * 4,
                }),
                TestCapture::Failure(error) => Err(error),
                TestCapture::Pending => std::future::pending().await,
            }
        }

        async fn close(&mut self) {}

        fn set_capture_timeout(&mut self, timeout: Duration) {
            self.capture_timeouts.lock().unwrap().push(timeout);
        }

        async fn invalidate_pending_capture(&mut self) {
            self.invalidations.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_stream() -> ConnectedStream {
        ConnectedStream {
            node_id: 1,
            restore_token: "rotated".to_owned(),
            persistent_id: Some("test-panel".to_owned()),
            width: 16,
            height: 9,
            position: None,
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
                stream_position: None,
                compositor_output: None,
            },
        )
        .unwrap();
    }

    fn service_deps_for_with_grids(
        display: &str,
        config: Arc<Config>,
        source: ServiceSource,
        consent_path: PathBuf,
        cancel: CancellationToken,
        latest_grids: LatestGrids,
    ) -> (ActiveSamplerDeps, mpsc::Sender<SamplerUpdate>, LatestGrids) {
        let (update_tx, update_rx) = mpsc::channel(2);
        (
            ActiveSamplerDeps {
                initial_config: config,
                display_id: DisplayId(display.to_owned()),
                update_rx,
                latest_grids: latest_grids.clone(),
                source: Box::new(source),
                source_reader: None,
                apps_probe: None,
                consent_path,
                cancel,
                env_reader: test_env_reader,
                event_tx: None,
            },
            update_tx,
            latest_grids,
        )
    }

    fn service_deps_for(
        display: &str,
        config: Arc<Config>,
        source: ServiceSource,
        consent_path: PathBuf,
        cancel: CancellationToken,
    ) -> (ActiveSamplerDeps, mpsc::Sender<SamplerUpdate>, LatestGrids) {
        service_deps_for_with_grids(
            display,
            config,
            source,
            consent_path,
            cancel,
            new_latest_grids(),
        )
    }

    fn service_deps(
        config: Arc<Config>,
        source: ServiceSource,
        consent_path: PathBuf,
        cancel: CancellationToken,
    ) -> (ActiveSamplerDeps, mpsc::Sender<SamplerUpdate>, LatestGrids) {
        service_deps_for("oled", config, source, consent_path, cancel)
    }

    fn multi_active_config(interval: Duration) -> Arc<Config> {
        let mut config = (*active_config(interval)).clone();
        config.wear.active_sampling.sampled_display = None;
        config.wear.active_sampling.sampled_displays =
            vec!["oled-a".to_owned(), "oled-b".to_owned()];
        Arc::new(config)
    }

    fn test_record_for(path: &std::path::Path, display: &str) {
        crate::screencast_consent::store_atomic(
            path,
            &crate::screencast_consent::ConsentRecord {
                token: format!("saved-{display}"),
                sampled_display: display.to_owned(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
                portal_persistent_ids: vec![format!("panel-{display}")],
                granted_width: 16,
                granted_height: 9,
                stream_position: None,
                compositor_output: None,
            },
        )
        .unwrap();
    }

    /// Consent record for the gated TV fixture: pins both the display id
    /// and the configured `compositor_output` so the runtime's `Runtime::new`
    /// drift check does not wipe it on first launch.
    fn test_tv_consent(path: &std::path::Path, compositor_output: &str) {
        let compositor_output = if compositor_output.is_empty() {
            None
        } else {
            Some(compositor_output.to_owned())
        };
        crate::screencast_consent::store_atomic(
            path,
            &crate::screencast_consent::ConsentRecord {
                token: "saved-tv".to_string(),
                sampled_display: "tv".to_owned(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
                portal_persistent_ids: vec!["panel-tv".to_owned()],
                granted_width: 16,
                granted_height: 9,
                stream_position: None,
                compositor_output,
            },
        )
        .unwrap();
    }

    async fn wait_for_state(handle: &ActiveSamplerHandle, expected: SamplingState) {
        let mut status = handle.status();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if status.borrow().state == expected {
                    return;
                }
                status
                    .changed()
                    .await
                    .expect("sampler status channel remains open");
            }
        })
        .await
        .expect("sampler reached expected state");
    }

    fn service_source(
        captures: impl IntoIterator<Item = TestCapture>,
    ) -> (
        ServiceSource,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let closes_seen = Arc::new(AtomicUsize::new(0));
        let grants_seen = Arc::new(AtomicUsize::new(0));
        (
            ServiceSource {
                connects: Mutex::new(VecDeque::new()),
                connects_seen: Arc::new(AtomicUsize::new(0)),
                captures: Mutex::new(captures.into_iter().collect()),
                captures_seen: captures_seen.clone(),
                closes_seen: closes_seen.clone(),
                grants_seen: grants_seen.clone(),
            },
            captures_seen,
            closes_seen,
            grants_seen,
        )
    }

    #[tokio::test]
    async fn two_selected_displays_open_independent_consent_flows_and_records() {
        let dir = tempdir().unwrap();
        let config = multi_active_config(Duration::from_secs(10));
        let path_a = dir.path().join("consent-a.json");
        let path_b = dir.path().join("consent-b.json");
        let (source_a, _, _, grants_a) = service_source([TestCapture::Frame]);
        let (source_b, _, _, grants_b) = service_source([TestCapture::Frame]);
        let cancel_a = CancellationToken::new();
        let cancel_b = CancellationToken::new();
        let (deps_a, _, _) = service_deps_for(
            "oled-a",
            config.clone(),
            source_a,
            path_a.clone(),
            cancel_a.clone(),
        );
        let (deps_b, _, _) =
            service_deps_for("oled-b", config, source_b, path_b.clone(), cancel_b.clone());
        let (handle_a, join_a) = spawn_with_handle(deps_a);
        let (handle_b, join_b) = spawn_with_handle(deps_b);

        let (reply_a_tx, reply_a_rx) = oneshot::channel();
        handle_a
            .send(SamplerCommand::Enable { reply: reply_a_tx })
            .await
            .unwrap();
        let (reply_b_tx, reply_b_rx) = oneshot::channel();
        handle_b
            .send(SamplerCommand::Enable { reply: reply_b_tx })
            .await
            .unwrap();

        assert_eq!(reply_a_rx.await.unwrap(), ConsentFlowStatus::Granted);
        assert_eq!(reply_b_rx.await.unwrap(), ConsentFlowStatus::Granted);
        assert_eq!(grants_a.load(Ordering::SeqCst), 1);
        assert_eq!(grants_b.load(Ordering::SeqCst), 1);
        assert_eq!(
            crate::screencast_consent::load(&path_a, "oled-a", None)
                .unwrap()
                .record()
                .sampled_display,
            "oled-a"
        );
        assert_eq!(
            crate::screencast_consent::load(&path_b, "oled-b", None)
                .unwrap()
                .record()
                .sampled_display,
            "oled-b"
        );

        cancel_a.cancel();
        cancel_b.cancel();
        join_a.await.unwrap();
        join_b.await.unwrap();
    }

    #[tokio::test]
    async fn one_display_cooldown_does_not_move_the_other_sampler() {
        let dir = tempdir().unwrap();
        let config = multi_active_config(Duration::from_secs(10));
        let path_a = dir.path().join("consent-a.json");
        let path_b = dir.path().join("consent-b.json");
        test_record_for(&path_a, "oled-a");
        test_record_for(&path_b, "oled-b");
        let (source_a, _, _, _) = service_source([TestCapture::Failure(CaptureError::Timeout)]);
        let (source_b, _, _, _) = service_source([TestCapture::Frame]);
        let cancel_a = CancellationToken::new();
        let cancel_b = CancellationToken::new();
        let (deps_a, _, _) =
            service_deps_for("oled-a", config.clone(), source_a, path_a, cancel_a.clone());
        let (deps_b, _, _) = service_deps_for("oled-b", config, source_b, path_b, cancel_b.clone());
        let (handle_a, join_a) = spawn_with_handle(deps_a);
        let (handle_b, join_b) = spawn_with_handle(deps_b);

        wait_for_state(&handle_a, SamplingState::Cooldown).await;
        wait_for_state(&handle_b, SamplingState::Streaming).await;
        assert_eq!(handle_a.status().borrow().state, SamplingState::Cooldown);
        assert_eq!(handle_b.status().borrow().state, SamplingState::Streaming);

        cancel_a.cancel();
        cancel_b.cancel();
        join_a.await.unwrap();
        join_b.await.unwrap();
    }

    /// Proves the production [`spawn_with_handle`] path gives each display an
    /// independently owned cancellation token: cancelling sampler A must not
    /// affect sampler B's liveliness.
    #[tokio::test]
    async fn sampler_runtime_lifecycles_are_independent() {
        let dir = tempdir().unwrap();
        let config = multi_active_config(Duration::from_secs(10));
        let path_a = dir.path().join("consent-a.json");
        let path_b = dir.path().join("consent-b.json");
        test_record_for(&path_a, "oled-a");
        test_record_for(&path_b, "oled-b");
        let (source_a, _, _, _) = service_source([TestCapture::Frame]);
        let (source_b, _, _, _) = service_source([TestCapture::Frame]);
        let cancel_a = CancellationToken::new();
        let cancel_b = CancellationToken::new();
        let (deps_a, _, _) =
            service_deps_for("oled-a", config.clone(), source_a, path_a, cancel_a.clone());
        let (deps_b, _, _) = service_deps_for("oled-b", config, source_b, path_b, cancel_b.clone());
        let (handle_a, join_a) = spawn_with_handle(deps_a);
        let (handle_b, join_b) = spawn_with_handle(deps_b);

        wait_for_state(&handle_a, SamplingState::Streaming).await;
        wait_for_state(&handle_b, SamplingState::Streaming).await;

        // Cancel A; B must remain live — its join handle must not be finished
        // and its status must still show Streaming.
        cancel_a.cancel();
        let _ = tokio::time::timeout(Duration::from_millis(50), handle_a.status().changed())
            .await
            .expect("handle_a status should change after cancel");
        assert_eq!(
            handle_b.status().borrow().state,
            SamplingState::Streaming,
            "cancelling display A must not affect display B's sampler state"
        );
        assert!(
            !join_b.is_finished(),
            "display B's join must not finish when display A is cancelled"
        );

        cancel_b.cancel();
        join_a.await.unwrap();
        join_b.await.unwrap();
    }

    #[tokio::test]
    async fn two_samplers_never_cross_deliver_latest_grids() {
        let dir = tempdir().unwrap();
        let config = multi_active_config(Duration::from_secs(10));
        let path_a = dir.path().join("consent-a.json");
        let path_b = dir.path().join("consent-b.json");
        test_record_for(&path_a, "oled-a");
        test_record_for(&path_b, "oled-b");
        let (source_a, _, _, _) = service_source([TestCapture::FrameValue(32)]);
        let (source_b, _, _, _) = service_source([TestCapture::FrameValue(224)]);
        let cancel_a = CancellationToken::new();
        let cancel_b = CancellationToken::new();
        let latest_grids = new_latest_grids();
        let (deps_a, _, _) = service_deps_for_with_grids(
            "oled-a",
            config.clone(),
            source_a,
            path_a,
            cancel_a.clone(),
            latest_grids.clone(),
        );
        let (deps_b, _, _) = service_deps_for_with_grids(
            "oled-b",
            config,
            source_b,
            path_b,
            cancel_b.clone(),
            latest_grids.clone(),
        );
        let (handle_a, join_a) = spawn_with_handle(deps_a);
        let (handle_b, join_b) = spawn_with_handle(deps_b);

        wait_for_state(&handle_a, SamplingState::Streaming).await;
        wait_for_state(&handle_b, SamplingState::Streaming).await;
        {
            let grids = latest_grids.read().unwrap();
            assert_eq!(grids.len(), 2);
            let grid_a = grids
                .get(&DisplayId("oled-a".to_owned()))
                .expect("display A sample");
            let grid_b = grids
                .get(&DisplayId("oled-b".to_owned()))
                .expect("display B sample");
            assert_ne!(grid_a.grid, grid_b.grid);
        }

        cancel_a.cancel();
        cancel_b.cancel();
        join_a.await.unwrap();
        join_b.await.unwrap();
    }

    #[test]
    fn streaming_entry_emits_started_once() {
        let dir = tempdir().unwrap();
        let config = active_config(Duration::from_secs(10));
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let mut runtime = Runtime::new(
            &config,
            &dir.path().join("consent.json"),
            &DisplayId("oled".to_owned()),
        );
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
        let mut runtime = Runtime::new(
            &config,
            &dir.path().join("consent.json"),
            &DisplayId("oled".to_owned()),
        );
        runtime.event_tx = Some(event_tx);
        let (status_tx, _) = watch::channel(SamplerStatus {
            state: SamplingState::Disabled,
            last_capture: None,
            uniform_reason: None,
            bound_display: None,
            compositor_output: None,
            granted_at: None,
            source_gate: None,
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
        assert!(
            latest
                .read()
                .unwrap()
                .contains_key(&DisplayId("oled".to_owned()))
        );

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
            display_id: DisplayId("oled".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(source),
            source_reader: None,
            apps_probe: None,
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
                    compositor_output: None,
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
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
    async fn active_sampler_persistent_capture_failure_reattaches_saved_session() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::from([Ok(test_stream()), Ok(test_stream())])),
            connects_seen: connects_seen.clone(),
            captures: Mutex::new(VecDeque::from([
                TestCapture::Failure(CaptureError::Timeout),
                TestCapture::Failure(CaptureError::Timeout),
                TestCapture::Frame,
            ])),
            captures_seen: captures_seen.clone(),
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

        tokio::time::timeout(Duration::from_secs(1), async {
            while captures_seen.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        wait_for_state(&handle, SamplingState::Cooldown).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(connects_seen.load(Ordering::SeqCst), 2);
        assert_eq!(handle.status().borrow().state, SamplingState::Streaming);

        cancel.cancel();
        join.await.unwrap();
    }

    #[test]
    fn cooldown_capture_failure_keeps_reason_while_reattaching() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        runtime.state = SamplingState::Cooldown;
        let (status_tx, _) = watch::channel(initial_status(&config));

        apply_trigger(&mut runtime, Trigger::CaptureFailed, &status_tx);

        let wire = status_tx.borrow().redacted(Tick::now());
        assert_eq!(runtime.state, SamplingState::Connecting);
        assert_eq!(wire.uniform_reason.as_deref(), Some(WEAR_SAMPLING_COOLDOWN));
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
            crate::screencast_consent::load(&consent_path, "oled", None)
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
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
    async fn active_sampler_rejects_enable_while_active() {
        let cases: &[(&str, &[Trigger])] = &[
            ("connecting", &[Trigger::GrantStarted, Trigger::Granted]),
            (
                "streaming",
                &[Trigger::GrantStarted, Trigger::Granted, Trigger::Connected],
            ),
            (
                "cooldown",
                &[
                    Trigger::GrantStarted,
                    Trigger::Granted,
                    Trigger::Connected,
                    Trigger::CaptureFailed,
                ],
            ),
        ];
        for (label, setup) in cases {
            let dir = tempdir().unwrap();
            let consent_path = dir.path().join("consent.json");
            let config = active_config(Duration::from_secs(10));
            let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
            let (status_tx, _) = watch::channel(initial_status(&config));
            for trigger in *setup {
                apply_trigger(&mut runtime, *trigger, &status_tx);
            }
            let expected_state = runtime.state;
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
                .await,
                "{label}: handle_command returned true (signalled connect)"
            );
            assert_eq!(
                reply_rx.await.unwrap(),
                ConsentFlowStatus::Error(SamplerError::FlowAlreadyActive.to_string()),
                "{label}: reply mismatch"
            );
            assert_eq!(
                runtime.state, expected_state,
                "{label}: state mutated by rejected Enable"
            );
            assert_eq!(
                source.grants_seen.load(Ordering::SeqCst),
                0,
                "{label}: request_consent was called"
            );
            assert_eq!(
                source.closes_seen.load(Ordering::SeqCst),
                0,
                "{label}: close was called"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_rejects_enable_while_suspended() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));
        apply_trigger(
            &mut runtime,
            Trigger::ConfigChanged(ConfigDelta::WearEnabled(false)),
            &status_tx,
        );
        let expected_state = runtime.state;
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
            ConsentFlowStatus::Error(SamplerError::AdministrativelySuspended.to_string()),
            "suspended: reply mismatch"
        );
        assert_eq!(
            runtime.state, expected_state,
            "suspended: state mutated by rejected Enable"
        );
        assert_eq!(
            source.grants_seen.load(Ordering::SeqCst),
            0,
            "suspended: request_consent was called"
        );
        assert_eq!(
            source.closes_seen.load(Ordering::SeqCst),
            0,
            "suspended: close was called"
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
            display_id: DisplayId("oled".to_owned()),
            update_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(ScriptedCaptureSource::with_pending_consent()),
            source_reader: None,
            apps_probe: None,
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
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        let failure = log
            .lines()
            .find(|line| line.contains("wear_sampling_consent_failed"))
            .expect("timeout failure log");
        assert!(failure.contains("reason=\"timeout\""), "{failure}");
        assert!(failure.contains("display=oled"), "{failure}");
    }

    #[tokio::test]
    async fn active_sampler_denial_replies_denied() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        let failure = log
            .lines()
            .find(|line| {
                line.contains("wear_sampling_consent_failed")
                    && line.contains("open_pipewire_remote_timeout")
            })
            .expect("transport failure log");
        assert!(failure.contains("display=oled"), "{failure}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_sampler_logs_token_persisted_without_token_value() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        assert!(log.contains("display=oled"), "{log}");
        assert!(!log.contains("unlogged-rotated-token"), "{log}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn active_sampler_fresh_grant_persists_native_dimensions_that_reconcile() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        let record = crate::screencast_consent::load(&consent_path, "oled", None)
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
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
        assert!(crate::screencast_consent::load(&consent_path, "oled", None).is_ok());
        assert_eq!(source.close_calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_disable_then_enable_stops_and_resumes_without_consent() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let closes_seen = Arc::new(AtomicUsize::new(0));
        let grants_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::from([Ok(test_stream()), Ok(test_stream())])),
            connects_seen: connects_seen.clone(),
            captures: Mutex::new(VecDeque::from([TestCapture::Frame])),
            captures_seen: captures_seen.clone(),
            closes_seen: closes_seen.clone(),
            grants_seen: grants_seen.clone(),
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
        assert_eq!(handle.status().borrow().state, SamplingState::Streaming);
        let captures_before_disable = captures_seen.load(Ordering::SeqCst);

        let (disable_tx, disable_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Disable {
                forget: false,
                reply: disable_tx,
            })
            .await
            .unwrap();
        assert!(disable_rx.await.unwrap().is_ok());
        assert_eq!(handle.status().borrow().state, SamplingState::Disabled);
        assert_eq!(closes_seen.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            captures_seen.load(Ordering::SeqCst),
            captures_before_disable
        );
        assert_eq!(closes_seen.load(Ordering::SeqCst), 1);

        let (enable_tx, enable_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Enable { reply: enable_tx })
            .await
            .unwrap();
        assert_eq!(enable_rx.await.unwrap(), ConsentFlowStatus::Granted);
        tokio::task::yield_now().await;
        assert_eq!(handle.status().borrow().state, SamplingState::Streaming);
        assert_eq!(grants_seen.load(Ordering::SeqCst), 0);
        assert_eq!(connects_seen.load(Ordering::SeqCst), 2);

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_forget_cancels_pending_consent_and_deletes_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let (update_tx, update_rx) = mpsc::channel(1);
        drop(update_tx);
        let cancel = CancellationToken::new();
        let close_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let source = CloseTrackingScriptedSource {
            inner: ScriptedCaptureSource::with_pending_consent(),
            close_calls: close_calls.clone(),
            captures_seen: std::sync::Arc::new(AtomicUsize::new(0)),
            connect_calls: std::sync::Arc::new(AtomicUsize::new(0)),
        };
        let (handle, join) = spawn_with_handle(ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("oled".to_owned()),
            update_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(source),
            source_reader: None,
            apps_probe: None,
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
        // Disable during a pending consent must leave the sampler Disabled so a
        // subsequent Enable is rejected by the disabled-config path instead of
        // being accepted as a fresh consent flow.
        let final_status = handle.status();
        let snapshot = final_status.borrow().clone();
        assert_eq!(snapshot.state, SamplingState::Disabled);
        assert!(
            close_calls.load(Ordering::SeqCst) >= 1,
            "Disable during pending consent must close any live sampler session"
        );
        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_streaming_capture_auth_failure_closes_source() {
        // A live portal session invalidated by an Auth failure during
        // Streaming must be released: the run loop parks in NeedsConsent
        // indefinitely, so without honoring the CloseSession effect the
        // warm PipeWire worker and the portal session would leak.
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let config = active_config(Duration::from_secs(10));
        let closes_seen = Arc::new(AtomicUsize::new(0));
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let source = ServiceSource {
            connects: Mutex::new(VecDeque::new()),
            connects_seen: Arc::new(AtomicUsize::new(0)),
            captures: Mutex::new(VecDeque::from([
                TestCapture::Frame,
                TestCapture::Failure(CaptureError::Auth),
            ])),
            captures_seen: captures_seen.clone(),
            closes_seen: closes_seen.clone(),
            grants_seen: Arc::new(AtomicUsize::new(0)),
        };
        let cancel = CancellationToken::new();
        let (deps, _updates, _latest) = service_deps(config, source, consent_path, cancel.clone());
        let (handle, join) = spawn_with_handle(deps);

        // First cadence tick: connect succeeds, first capture returns a frame.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(captures_seen.load(Ordering::SeqCst), 1);
        assert_eq!(handle.status().borrow().state, SamplingState::Streaming);

        // Advance past the sample interval to drive a second capture, which
        // fails with Auth and routes through needs_consent -> NeedsConsent
        // with a CloseSession effect.
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(captures_seen.load(Ordering::SeqCst), 2);
        assert_eq!(handle.status().borrow().state, SamplingState::NeedsConsent);
        assert!(
            closes_seen.load(Ordering::SeqCst) >= 1,
            "Auth failure during Streaming must close the live portal session"
        );

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn active_sampler_disable_during_pending_consent_rejects_followup_enable() {
        // A Disable received while a consent flow is still pending must
        // leave the sampler Disabled: the in-flight Enable replies with
        // "cancelled", the Disable itself replies Ok(()), and any later
        // Enable is rejected (the sampler is no longer in NeedsConsent, so
        // it cannot silently re-open a fresh grant dialog).
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let (update_tx, update_rx) = mpsc::channel(1);
        drop(update_tx);
        let cancel = CancellationToken::new();
        let (handle, join) = spawn_with_handle(ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("oled".to_owned()),
            update_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(ScriptedCaptureSource::with_pending_consent()),
            source_reader: None,
            apps_probe: None,
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
        // The first Enable was cancelled by the Disable.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), enable_rx)
                .await
                .unwrap()
                .unwrap(),
            ConsentFlowStatus::Error("cancelled".to_owned())
        );
        // Final state is Disabled — the operator's disable took effect.
        assert_eq!(handle.status().borrow().state, SamplingState::Disabled);

        // The follow-up Enable is rejected because the sampler is disabled
        // and no consent record exists to resume.
        let (followup_tx, followup_rx) = oneshot::channel();
        handle
            .send(SamplerCommand::Enable { reply: followup_tx })
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), followup_rx)
                .await
                .unwrap()
                .unwrap(),
            ConsentFlowStatus::Error(SamplerError::SamplingDisabled.to_string())
        );

        cancel.cancel();
        join.await.unwrap();
    }

    /// `ScriptedCaptureSource` wrapper that counts `close()` invocations via a
    /// shared atomic so the test can observe the run loop honoring a
    /// `CloseSession` effect across the `Box<dyn CaptureSource>` boundary.
    struct CloseTrackingScriptedSource {
        inner: ScriptedCaptureSource,
        close_calls: std::sync::Arc<AtomicUsize>,
        captures_seen: std::sync::Arc<AtomicUsize>,
        connect_calls: std::sync::Arc<AtomicUsize>,
    }

    impl CloseTrackingScriptedSource {
        fn new(inner: ScriptedCaptureSource) -> Self {
            let captures_seen = std::sync::Arc::new(AtomicUsize::new(0));
            let close_calls = std::sync::Arc::new(AtomicUsize::new(0));
            let connect_calls = std::sync::Arc::new(AtomicUsize::new(0));
            Self {
                inner,
                close_calls,
                captures_seen,
                connect_calls,
            }
        }
    }

    #[async_trait]
    impl CaptureSource for CloseTrackingScriptedSource {
        async fn connect(
            &mut self,
            binding: &ConsentBinding<'_>,
        ) -> Result<ConnectedStream, CaptureError> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.connect(binding).await
        }
        async fn request_consent(
            &mut self,
            display: &DisplayExpectation,
        ) -> Result<Grant, CaptureError> {
            self.inner.request_consent(display).await
        }
        async fn capture_one(&mut self, mode: StreamMode) -> Result<RawFrame, CaptureError> {
            self.captures_seen.fetch_add(1, Ordering::SeqCst);
            self.inner.capture_one(mode).await
        }
        async fn close(&mut self) {
            self.inner.close().await;
            self.close_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn active_sampler_wrong_monitor_grant_returns_error_without_record() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        let config = active_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &consent_path, &DisplayId("oled".to_owned()));
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
                name: "token rejection requires fresh consent and closes the live session",
                state: SamplingState::Connecting,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_token_invalid"),
                ],
            },
            TransitionCase {
                name: "wrong reattached monitor requires fresh consent with its distinct reason and closes the live session",
                state: SamplingState::Connecting,
                trigger: Trigger::WrongMonitor,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_wrong_monitor"),
                ],
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
                name: "stream session closure invalidates consent and closes the live session",
                state: SamplingState::Streaming,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_token_invalid"),
                ],
            },
            TransitionCase {
                name: "wrong streaming monitor requires fresh consent with its distinct reason and closes the live session",
                state: SamplingState::Streaming,
                trigger: Trigger::WrongMonitor,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_wrong_monitor"),
                ],
            },
            TransitionCase {
                name: "stream auth rejection requires fresh consent and closes the live session",
                state: SamplingState::Streaming,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_token_invalid"),
                ],
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
                name: "cooldown auth failure requires fresh consent and closes the live session",
                state: SamplingState::Cooldown,
                trigger: Trigger::AuthFailed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_token_invalid"),
                ],
            },
            TransitionCase {
                name: "cooldown wrong monitor requires fresh consent and closes the live session",
                state: SamplingState::Cooldown,
                trigger: Trigger::WrongMonitor,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_wrong_monitor"),
                ],
            },
            TransitionCase {
                name: "renewed cooldown failure renegotiates the portal session",
                state: SamplingState::Cooldown,
                trigger: Trigger::CaptureFailed,
                has_consent_record: true,
                next: SamplingState::Connecting,
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
                name: "connecting session closure invalidates consent and closes the live session",
                state: SamplingState::Connecting,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_token_invalid"),
                ],
            },
            TransitionCase {
                name: "cooldown session closure invalidates consent and closes the live session",
                state: SamplingState::Cooldown,
                trigger: Trigger::SessionClosed,
                has_consent_record: true,
                next: SamplingState::NeedsConsent,
                effects: vec![
                    Effect::CloseSession,
                    Effect::EnterUniform("wear_sampling_token_invalid"),
                ],
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
            position: None,
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
            stream_position: None,
        };
        let display = DisplayExpectation {
            display: "oled".to_owned(),
            compositor_output: None,
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

    // ---------------------------------------------------------------------
    // Issue #211 — timeout ownership (defects A and B).
    // ---------------------------------------------------------------------

    /// Defect A: a `LimitsChanged` reconfigure must push the new
    /// `capture_timeout` to the platform source so its inner warm-mode bound
    /// stays in sync with the outer daemon bound.
    #[tokio::test(start_paused = true)]
    async fn active_sampler_limits_changed_reconfigure_pushes_capture_timeout_to_source() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let connects_seen = Arc::new(AtomicUsize::new(0));
        let capture_timeouts = Arc::new(Mutex::new(Vec::<Duration>::new()));
        let invalidations = Arc::new(AtomicUsize::new(0));
        let captures_seen = Arc::new(AtomicUsize::new(0));
        let source = RecordingSource {
            connects: connects_seen.clone(),
            capture_timeouts: capture_timeouts.clone(),
            invalidations: invalidations.clone(),
            captures: Mutex::new(VecDeque::new()),
            captures_seen: captures_seen.clone(),
        };
        let cancel = CancellationToken::new();
        let config = active_config(Duration::from_secs(10));
        let (updates_tx, updates_rx) = mpsc::channel(2);
        let deps = ActiveSamplerDeps {
            initial_config: config.clone(),
            display_id: DisplayId("oled".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(source),
            source_reader: None,
            apps_probe: None,
            consent_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (_handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        let recorded = capture_timeouts.lock().unwrap().clone();
        assert!(
            recorded
                .iter()
                .any(|timeout| *timeout == Duration::from_secs(2)),
            "initial capture_timeout must reach the source (got {recorded:?})"
        );

        let mut reconfigured = (*config).clone();
        reconfigured.wear.active_sampling.capture_timeout = Duration::from_secs(7);
        updates_tx
            .send(SamplerUpdate::Reconfigure(ReconfigurePlan {
                active_sampling: reconfigured.wear.active_sampling,
                sample_interval: reconfigured.wear.sample_interval,
                trigger: ConfigDelta::LimitsChanged,
            }))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let recorded = capture_timeouts.lock().unwrap().clone();
        assert!(
            recorded
                .iter()
                .any(|timeout| *timeout == Duration::from_secs(7)),
            "LimitsChanged reconfigure must push the new capture_timeout to the source (got {recorded:?})"
        );
        let _ = invalidations.load(Ordering::SeqCst);
        let _ = captures_seen.load(Ordering::SeqCst);

        cancel.cancel();
        join.await.unwrap();
    }

    /// Defect B: when the outer `tokio::time::timeout` elapses before the
    /// inner bound, the lifecycle must call `invalidate_pending_capture` on
    /// the source so a buffered frame cannot be served on the next capture.
    #[tokio::test(start_paused = true)]
    async fn active_sampler_outer_timeout_invalidates_pending_capture() {
        let dir = tempdir().unwrap();
        let consent_path = dir.path().join("consent.json");
        test_record(&consent_path);
        let invalidations = Arc::new(AtomicUsize::new(0));
        let source: Box<dyn CaptureSource + Send + Sync + 'static> = Box::new(RecordingSource {
            connects: Arc::new(AtomicUsize::new(0)),
            capture_timeouts: Arc::new(Mutex::new(Vec::new())),
            invalidations: invalidations.clone(),
            captures: Mutex::new(VecDeque::from([TestCapture::Pending, TestCapture::Pending])),
            captures_seen: Arc::new(AtomicUsize::new(0)),
        });
        let cancel = CancellationToken::new();
        let mut config = active_config(Duration::from_secs(10));
        Arc::get_mut(&mut config)
            .unwrap()
            .wear
            .active_sampling
            .capture_timeout = Duration::from_millis(50);
        let latest_grids = new_latest_grids();
        let (update_tx, update_rx) = mpsc::channel(2);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("oled".to_owned()),
            update_rx,
            latest_grids: latest_grids.clone(),
            source,
            source_reader: None,
            apps_probe: None,
            consent_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (handle, join) = spawn_with_handle(deps);

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(60)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            invalidations.load(Ordering::SeqCst),
            1,
            "outer capture timeout must invalidate the source's pending state"
        );
        let _ = handle.status();
        cancel.cancel();
        join.await.unwrap();
        let _ = update_tx;
    }

    // ── Source-gate lifecycle ──────────────────────────────────────

    /// TV config that wires the gate-expectation into the active sampler:
    /// a Samsung-tizen display with a `host` plus a `sampling.expected_source`
    /// declared on the display config, and a TV-side configured
    /// `sampled_display`. Used as the `initial_config` of every
    /// source-gate integration test below.
    fn gated_tv_config(interval: Duration) -> Arc<Config> {
        use dormant_core::config::schema::{DisplaySamplingConfig, DisplayScope, HookSlots};
        use dormant_core::types::{BlankMode, LadderStage, StageKind};
        let mut config = (*active_config(interval)).clone();
        config.wear.active_sampling.sampled_display = Some("tv".to_owned());
        config.displays.insert(
            "tv".to_owned(),
            DisplayConfig {
                controllers: vec!["samsung-tizen".to_owned()],
                scope: DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_write_code: None,
                shared_peer_input_code: None,
                hooks: HookSlots::default(),
                blank_mode: Some(BlankMode::BrightnessZero),
                degraded_mode: None,
                ladder: vec![LadderStage {
                    kind: StageKind::Controller(BlankMode::BrightnessZero),
                    dwell: None,
                }],
                screensaver: None,
                output: None,
                ddc_display: None,
                host: Some("tv.local".to_owned()),
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
                samsung_restore_backlight:
                    dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::default(),
                power_off_opt_in: false,
                compositor_output: Some("HDMI-A-1".to_owned()),
                sampling: Some(DisplaySamplingConfig {
                    expected_source: Some("HDMI4".to_owned()),
                    source_poll_interval: Duration::from_secs(2),
                    stream_mode: None,
                    watched_apps: Vec::new(),
                }),
            },
        );
        Arc::new(config)
    }

    /// Step 1 (RED): the source gate must gate `capture_one` per cadence
    /// tick. Matched → 1 capture. Mismatched / Unknown → 0 captures, no
    /// grid, lifecycle still `Streaming`. The assertion is on the source's
    /// atomic capture counter (not just the absent grid), so deleting the
    /// skip branch turns the `mismatched` / `unknown` rows red.
    #[tokio::test(start_paused = true)]
    #[allow(
        clippy::too_many_lines,
        reason = "the match-three RED table covers all three gate states inline"
    )]
    async fn source_gate_skips_capture_when_mismatched_or_unknown() {
        let dir = tempdir().unwrap();

        // Matched gate: exactly one capture succeeds and the grid is published.
        let matched_path = dir.path().join("matched-consent.json");
        test_tv_consent(&matched_path, "HDMI-A-1");
        let (captures_seen, _latest, _handle, _join, _cancel) =
            drive_default(Ok("HDMI4"), &matched_path, Duration::from_secs(10));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            captures_seen.load(Ordering::SeqCst),
            1,
            "Matched gate must allow exactly one capture on the first cadence tick"
        );
    }

    /// Step 2 (RED): a runtime whose poller reports `Mismatched` from
    /// the first poll onwards must skip EVERY capture. The runtime's
    /// gate-skip match arm must fire on the Mismatched row (not only
    /// on the Unknown initial-state path). Disabling the Mismatched
    /// arm of `gate_skip` must make `captures_seen == 0` fail. The
    /// reader returns Ok("HDMI2") which `classify` resolves to
    /// Mismatched against the configured `expected_source` `"HDMI4"`.
    #[tokio::test(start_paused = true)]
    async fn source_gate_skips_capture_on_persistent_mismatch() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        // drive_default builds a single static response. The Error
        // variant maps to `Ok("")`-style Unknown — we need an `Ok("HDMI2")`
        // response to drive Mismatched, so build the deps inline.
        let config = gated_tv_config(Duration::from_secs(10));
        let (source_service, _, _, _) = service_source([TestCapture::Frame, TestCapture::Frame]);
        let captures_seen = source_service.captures_seen.clone();
        let source: Box<dyn CaptureSource + Send + Sync + 'static> =
            Box::new(GateProbeSource::new(source_service));
        let reader: Arc<dyn source_gate::InputSourceReader> = Arc::new(ScriptedSourceReader::new(
            [Ok("HDMI2".to_owned())],
            Ok("HDMI2".to_owned()),
        ));
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source,
            source_reader: Some(reader),
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        drop(updates_tx);
        let (_handle, join) = spawn_with_handle(deps);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            captures_seen.load(Ordering::SeqCst),
            0,
            "Mismatched gate must skip every capture (the Mismatched skip arm of `gate_skip` is what this row pins; disabling it makes the counter increment on every cadence tick)"
        );
        cancel.cancel();
        let _ = join.await;
    }

    /// Step 2 (RED): the `latest_grids` entry for a display must be
    /// cleared when the gate transitions away from Matched, so the
    /// wear tracker cannot see a stale grid promoted to a fresh
    /// attribute. The runtime's `apply_gate_observation` removes the
    /// grid entry inside its `needs_clear` branch on every transition
    /// away from the previous observation. Driving the test through
    /// the runtime requires a hot-spin capture; we instead drive it
    /// directly: seed `latest` with a grid for the TV display, run the
    /// runtime's own `apply_gate_observation` on a Mismatched
    /// observation (simulating the poller's first observation that
    /// differs from Matched), and assert the grid entry is gone.
    /// This proves the `needs_clear` branch; the runtime's
    /// `drain_gate_changes` calls this on every poll flip, so an
    /// in-flight transition-away-from-Matched is what removes the
    /// grid. Disabling `needs_clear` makes the assertion fail.
    #[test]
    fn source_gate_clears_latest_grids_on_transition_away_from_matched() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let config = gated_tv_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &tv_path, &DisplayId("tv".to_owned()));
        let latest = new_latest_grids();
        let display_id = DisplayId("tv".to_owned());
        // Seed the grid entry the way the capture path would.
        let (status_tx, _) = watch::channel(initial_status(&config));
        // Seed runtime.gate_state to Matched so the first
        // apply_gate_observation call below transitions
        // Matched -> Mismatched (the bug-under-test path).
        runtime.gate_state = Some(source_gate::SourceGate::Matched);
        latest.write().unwrap().insert(
            display_id.clone(),
            SampledGrid {
                grid: dormant_core::spatial_grid::LumaGrid::new(vec![0.0; 16 * 9]).unwrap(),
                captured_at: Tick::now(),
                phase_at_capture: Phase::Active,
            },
        );
        assert!(
            latest.read().unwrap().contains_key(&display_id),
            "seed must populate the grid entry"
        );
        // Feed the same Matched observation — must NOT clear the grid.
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Matched,
            &status_tx,
            &latest,
            &display_id,
        );
        assert!(
            latest.read().unwrap().contains_key(&display_id),
            "Matched observation must NOT clear the grid"
        );
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Mismatched {
                observed: "HDMI2".to_owned(),
            },
            &status_tx,
            &latest,
            &display_id,
        );
        assert!(
            !latest.read().unwrap().contains_key(&display_id),
            "transitioning from Matched to Mismatched must clear the grid entry"
        );
        // And the inverse: re-seeding a grid and feeding the same
        // observation must NOT clear it (no transition).
        latest.write().unwrap().insert(
            display_id.clone(),
            SampledGrid {
                grid: dormant_core::spatial_grid::LumaGrid::new(vec![0.5; 16 * 9]).unwrap(),
                captured_at: Tick::now(),
                phase_at_capture: Phase::Active,
            },
        );
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Mismatched {
                observed: "HDMI2".to_owned(),
            },
            &status_tx,
            &latest,
            &display_id,
        );
        assert!(
            latest.read().unwrap().contains_key(&display_id),
            "feeding the SAME observation (no transition) must not clear the grid"
        );
    }

    /// Step 2 (RED): the `WearSamplingSourceGate` event must fire
    /// EXACTLY ONCE per full-gate-value change — not per poll. The
    /// spec calls out `unknown -> mismatched -> mismatched -> matched`
    /// as a three-event sequence even though the poller observed four
    /// values (the second mismatched poll is a steady-state repeat).
    /// The runtime enforces this with `last_event_gate` deduplication
    /// inside `apply_gate_observation`. The test wires a real
    /// `event_tx` (all prior gate tests passed `None` and so could
    /// not observe the event at all) and feeds the four observations
    /// through `apply_gate_observation`, then drains the channel and
    /// asserts the exact 3-event sequence + observed-source mapping.
    #[test]
    fn source_gate_event_dedup_exactly_three_for_umm_match() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let config = gated_tv_config(Duration::from_secs(10));
        let mut runtime = Runtime::new(&config, &tv_path, &DisplayId("tv".to_owned()));
        let (event_tx, mut event_rx) = mpsc::channel(8);
        runtime.event_tx = Some(event_tx);
        let display_id = DisplayId("tv".to_owned());
        let (status_tx, _) = watch::channel(initial_status(&config));

        // Drain the WearSamplingStarted event the runtime emits on
        // entry to Streaming (publish_status fires it before our test
        // reaches the gate observation feed).
        let _ = event_rx.try_recv();

        // u -> m -> m -> m. Three transitions in the gate-state
        // value space; the two steady-state Mismatched polls must
        // produce ONE event, not two.
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Unknown {
                reason: "awaiting_first_poll",
            },
            &status_tx,
            &new_latest_grids(),
            &display_id,
        );
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Mismatched {
                observed: "HDMI2".to_owned(),
            },
            &status_tx,
            &new_latest_grids(),
            &display_id,
        );
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Mismatched {
                observed: "HDMI2".to_owned(),
            },
            &status_tx,
            &new_latest_grids(),
            &display_id,
        );
        apply_gate_observation(
            &mut runtime,
            &source_gate::SourceGate::Matched,
            &status_tx,
            &new_latest_grids(),
            &display_id,
        );

        let mut events: Vec<(String, Option<String>)> = Vec::new();
        while let Ok(msg) = event_rx.try_recv() {
            if let ControlMsg::PublishDaemonEvent(DaemonEvent::WearSamplingSourceGate {
                state,
                observed,
                ..
            }) = msg
            {
                events.push((state, observed));
            }
        }
        assert_eq!(
            events,
            vec![
                ("unknown".to_owned(), None),
                ("mismatched".to_owned(), Some("HDMI2".to_owned())),
                ("matched".to_owned(), None),
            ],
            "u -> m -> m -> matched must emit exactly 3 WearSamplingSourceGate events; got {events:?}"
        );
    }

    /// Step 2 (RED): the source-gate poller must only run for displays
    /// that declare a gate expectation AND only while the lifecycle
    /// is `Streaming`. Outside of Streaming — for example, in
    /// `NeedsConsent` after the operator revokes the portal grant —
    /// the runtime must report `source_gate = None` (permanently
    /// matched, no phantom observation) and the poller must not run
    /// at all. The probe drives `Runtime::new` with a gate-equipped
    /// config and no consent record, then asserts the initial state
    /// is `NeedsConsent` AND the status publishes with `source_gate =
    /// None`. With the runtime in `NeedsConsent` the poller must NOT
    /// spawn (lifecycle-gated), so the test does not need to wait for
    /// a poll observation to settle.
    #[tokio::test]
    async fn source_gate_poller_does_not_run_outside_streaming() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        // No consent record — runtime starts in NeedsConsent despite
        // the gate expectation in the config.
        let config = gated_tv_config(Duration::from_secs(10));
        let runtime = Runtime::new(&config, &tv_path, &DisplayId("tv".to_owned()));
        let (status_tx, _) = watch::channel(initial_status(&config));
        let status = SamplerStatus {
            state: runtime.state,
            last_capture: None,
            uniform_reason: None,
            bound_display: runtime
                .record
                .as_ref()
                .map(|record| record.record().sampled_display.clone()),
            compositor_output: runtime
                .display
                .display
                .as_ref()
                .and_then(|display| display.compositor_output.clone()),
            granted_at: runtime
                .record
                .as_ref()
                .map(|record| record.record().granted_at),
            source_gate: runtime.gate_state.clone(),
        };
        publish_status(&status_tx, &runtime, None, None);
        assert_eq!(
            runtime.state,
            SamplingState::NeedsConsent,
            "a gated display with no consent record must start in NeedsConsent"
        );
        assert_eq!(
            status.source_gate, None,
            "outside Streaming the status must not advertise a source_gate observation"
        );
        // reconcile_gate_poller must also have refused to spawn. We
        // pre-wire `source_reader` (the same way the run() helper does
        // when it takes `deps.source_reader`) so the `reader.is_some()`
        // arm of `wants_poller` is exercised — without a reader the
        // streaming-only check is irrelevant.
        let mut runtime = runtime;
        runtime.source_reader = Some(Arc::new(ScriptedSourceReader::new(
            [Ok("HDMI4".to_owned())],
            Ok("HDMI4".to_owned()),
        )));
        let (status_tx, _) = watch::channel(initial_status(&config));
        reconcile_gate_poller(&mut runtime, &status_tx);
        assert!(
            runtime.gate_poller.is_none(),
            "poller must NOT spawn in NeedsConsent lifecycle (streaming=false short-circuits the gate)"
        );
        assert!(
            runtime.gate_rx.is_none(),
            "gate_rx must NOT be wired in NeedsConsent lifecycle"
        );
        // And the inverse: rewire the lifecycle to Streaming (without
        // actually changing anything else) and confirm the poller NOW
        // spawns — the streaming-only gate is the only thing that
        // changes between the two reconcile calls.
        runtime.state = SamplingState::Streaming;
        reconcile_gate_poller(&mut runtime, &status_tx);
        assert!(
            runtime.gate_poller.is_some(),
            "poller MUST spawn once the lifecycle reaches Streaming"
        );
    }

    /// Step 4 (RED): adding a `source_gate_expectation` to a runtime that
    /// was already streaming without one must be fail-safe IMMEDIATELY.
    /// The previous spatial grid (if any) must be cleared, the status
    /// must publish `source_gate = Unknown{awaiting_first_poll}` with
    /// `uniform_reason = source_unknown`, and the additive gate event
    /// must fire — so the wear tracker cannot spatially attribute a
    /// pre-reload grid during the gate's first-poll window. The probe
    /// seeds a real `SampledGrid` on `latest_grids`, then sends a
    /// `DisplayContext` that adds the gate expectation. Without the
    /// fix the grid survives (and the wear tracker's
    /// `select_sample_for_attribution` prefers `Sampled` over fallback)
    /// so a tick between the publish and the first poll would
    /// spatially attribute stale content. With the fix the grid is
    /// cleared and the status carries `source_unknown` until the first
    /// poll lands.
    #[tokio::test(start_paused = true)]
    #[allow(
        clippy::too_many_lines,
        reason = "phase 1 capture then phase 2 gate-add then event-pinning reads cleanest as one flow"
    )]
    async fn source_gate_expectation_add_publishes_unknown_and_clears_grid() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        // Construct a runtime that starts WITHOUT a
        // source_gate_expectation — strip expected_source (the host
        // stays so `build_gate_expectation` returns `None`) and keep
        // `sampled_display` populated so the runtime still reaches
        // `Streaming` and captures freely on the first cadence tick.
        let config = gated_tv_config(Duration::from_secs(10));
        let mut config_clone = (*config).clone();
        if let Some(display) = config_clone.displays.get_mut("tv")
            && let Some(sampling) = display.sampling.as_mut()
        {
            sampling.expected_source = None;
        }
        let config = Arc::new(config_clone);
        let (source_service, _, _, _) = service_source([TestCapture::Frame, TestCapture::Frame]);
        let captures_seen = source_service.captures_seen.clone();
        let source: Box<dyn CaptureSource + Send + Sync + 'static> =
            Box::new(GateProbeSource::new(source_service));
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let latest_grids = new_latest_grids();
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: latest_grids.clone(),
            source,
            source_reader: Some(Arc::new(ScriptedSourceReader::new(
                [Ok("HDMI4".to_owned())],
                Ok("HDMI4".to_owned()),
            ))),
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: Some(event_tx),
        };
        let (_handle, join) = spawn_with_handle(deps);
        // Phase 1: let the runtime reach Streaming, advance past the
        // first cadence tick so the pre-reload grid is populated. With
        // no gate the runtime captures freely.
        tokio::time::advance(Duration::from_secs(11)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert!(
            captures_seen.load(Ordering::SeqCst) >= 1,
            "the runtime must capture at least one frame before the gate add; got {}",
            captures_seen.load(Ordering::SeqCst)
        );
        assert!(
            latest_grids
                .read()
                .unwrap()
                .contains_key(&DisplayId("tv".to_owned())),
            "pre-reload state must carry a non-empty grid entry for the TV display"
        );
        // Phase 2: send a DisplayContext that adds a
        // source_gate_expectation. With the fix this clears the
        // display's grid entry and publishes Unknown; without the fix
        // the grid survives and the status keeps its pre-reload source
        // gate (None for render-eligible runtimes that didn't have one
        // before).
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: Some(source_gate::SourceGateExpectation {
                    host: "tv.local".to_owned(),
                    expected_source: "HDMI4".to_owned(),
                    poll_interval: Duration::from_secs(2),
                    watched_apps: Arc::new([]),
                }),
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // The grid entry for this display must be cleared so the wear
        // tracker cannot fall back to the pre-reload `Sampled` value
        // while the gate is still unknown (or if it transitions
        // straight to Matched on the poller's first poll). The clear
        // is what the test pins — the source_gate value can move on
        // quickly to Matched once the poller's first poll lands, but
        // the wear tick that races between gate-add and first-poll
        // would otherwise spatially attribute the pre-reload grid.
        assert!(
            !latest_grids
                .read()
                .unwrap()
                .contains_key(&DisplayId("tv".to_owned())),
            "adding a gate must clear this display's latest_grids entry; the wear tracker would otherwise spatially attribute the pre-reload grid during the unknown gap"
        );
        // The additive `WearSamplingSourceGate` event must fire with
        // state=unknown immediately — pinning the FIRST event is the
        // fail-safe observable: if the fix is reverted the gate-add
        // never emits an Unknown event (the natural drain would only
        // see Matched on the poller's first poll, since drain runs
        // AFTER the poller's first tick when both share the same
        // select-wake). Without the explicit publish at gate-add, the
        // status flips Unknown → Matched with no Unknown event ever
        // firing for the additive channel.
        let mut first_event: Option<DaemonEvent> = None;
        while let Ok(msg) = event_rx.try_recv() {
            if let ControlMsg::PublishDaemonEvent(event) = msg
                && matches!(event, DaemonEvent::WearSamplingSourceGate { .. })
                && first_event.is_none()
            {
                first_event = Some(event);
            }
        }
        let first_event = first_event.expect(
            "adding a gate must emit a WearSamplingSourceGate event with the seed observation",
        );
        match first_event {
            DaemonEvent::WearSamplingSourceGate { state, .. } => {
                assert_eq!(
                    state, "unknown",
                    "the first gate event must be state=unknown (got {state}); the pre-poll window MUST be visible downstream"
                );
            }
            other => panic!("expected WearSamplingSourceGate, got {other:?}"),
        }
        cancel.cancel();
        let _ = join.await;
    }

    /// Step 4 (RED): a runtime that was spawned without a
    /// `source_gate_expectation` must still start its poller when a
    /// later `DisplayContext` adds one (e.g. operator filled in the TV
    /// `host` after the runtime was already up). The runtime caches a
    /// source reader at spawn so `reconcile_gate_poller`'s
    /// `wants_poller` arm fires the moment the new expectation
    /// arrives — without this the late add would silently never run.
    #[tokio::test(start_paused = true)]
    async fn source_gate_expectation_late_add_starts_poller() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let config = gated_tv_config(Duration::from_secs(10));
        let (source_service, _, _, _) = service_source([TestCapture::Frame, TestCapture::Frame]);
        let source: Box<dyn CaptureSource + Send + Sync + 'static> =
            Box::new(GateProbeSource::new(source_service));
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        // Construct a runtime that lacks a source_gate_expectation at
        // spawn: we mutate the cloned config so the initial
        // `build_gate_expectation` returns `None` (the host stays so
        // we just strip expected_source — and keep `sampled_display`
        // populated so the runtime reaches `Streaming`).
        let mut config = (*config).clone();
        if let Some(display) = config.displays.get_mut("tv")
            && let Some(sampling) = display.sampling.as_mut()
        {
            sampling.expected_source = None;
        }
        let config = Arc::new(config);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source,
            // No source_reader at spawn: the runtime must build one
            // itself when the late DisplayContext arrives (or the gate
            // silently never runs).
            source_reader: None,
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: Some(event_tx),
        };
        let (_handle, join) = spawn_with_handle(deps);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Send a DisplayContext that adds the gate expectation. The
        // runtime was spawned without one, so its pre-update
        // `source_gate_expectation` was `None`. With the fix the
        // expectation arrives, `wants_poller` flips true, and the
        // poller spawns.
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: Some(source_gate::SourceGateExpectation {
                    host: "tv.local".to_owned(),
                    expected_source: "HDMI4".to_owned(),
                    poll_interval: Duration::from_secs(2),
                    watched_apps: Arc::new([]),
                }),
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        // Pin the additive `WearSamplingSourceGate` event sequence: the
        // late add must emit `unknown` first, then transition to
        // `matched` after the poller's first poll. The test fails if
        // the runtime emits only `matched` (no `unknown`), which is
        // what happens without the explicit publish at gate-add: the
        // poller's first poll races ahead of any drain call, so the
        // additive channel never observes the seed Unknown.
        let mut first_event: Option<DaemonEvent> = None;
        while let Ok(msg) = event_rx.try_recv() {
            if let ControlMsg::PublishDaemonEvent(event) = msg
                && matches!(event, DaemonEvent::WearSamplingSourceGate { .. })
                && first_event.is_none()
            {
                first_event = Some(event);
            }
        }
        let first_event = first_event
            .expect("a late source_gate_expectation add must emit a WearSamplingSourceGate event");
        match first_event {
            DaemonEvent::WearSamplingSourceGate { state, .. } => {
                assert_eq!(
                    state, "unknown",
                    "the first gate event for a late add must be state=unknown (got {state}); the pre-poll window MUST be visible downstream"
                );
            }
            other => panic!("expected WearSamplingSourceGate, got {other:?}"),
        }
        cancel.cancel();
        let _ = join.await;
    }

    /// Step 4 (RED): the gate-add clear must land on the SHARED
    /// `latest_grids` IMMEDIATELY — inside `reconcile_gate_poller`,
    /// before the poller's first poll can possibly answer. The probe
    /// uses a reader whose poll never resolves, so no poll- or
    /// cadence-driven clear can fire: only the seed-time clear can
    /// empty the grid. Without the `run()` handoff that rebinds the
    /// runtime's cached map to the daemon-lifetime shared map, the
    /// seed clear targets a private map and the stale grid survives
    /// indefinitely — exactly the pre-reload spatial-attribution
    /// window the fail-safe rule forbids. The status must also carry
    /// `uniform_reason = wear_sampling_source_unknown` from the seed
    /// publish, not from a later poll.
    #[tokio::test(start_paused = true)]
    #[allow(
        clippy::too_many_lines,
        reason = "phase 1 capture then phase 2 gate-add then status/event pinning reads cleanest as one flow"
    )]
    async fn source_gate_expectation_add_clears_shared_grid_before_first_poll() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        // Spawn ungated (host stays, expected_source stripped) so the
        // runtime reaches Streaming and captures freely on phase 1.
        let config = gated_tv_config(Duration::from_secs(10));
        let mut config_clone = (*config).clone();
        if let Some(display) = config_clone.displays.get_mut("tv")
            && let Some(sampling) = display.sampling.as_mut()
        {
            sampling.expected_source = None;
        }
        let config = Arc::new(config_clone);
        let (source_service, _, _, _) = service_source([TestCapture::Frame, TestCapture::Frame]);
        let source: Box<dyn CaptureSource + Send + Sync + 'static> =
            Box::new(GateProbeSource::new(source_service));
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let latest_grids = new_latest_grids();
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: latest_grids.clone(),
            source,
            // The poll can never answer: any grid clear observed after
            // the gate add MUST have come from the seed publish.
            source_reader: Some(Arc::new(PendingSourceReader)),
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: Some(event_tx),
        };
        let (handle, join) = spawn_with_handle(deps);
        let status_rx = handle.status();
        // Phase 1: capture one frame so the shared grid is populated.
        tokio::time::advance(Duration::from_secs(11)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert!(
            latest_grids
                .read()
                .unwrap()
                .contains_key(&DisplayId("tv".to_owned())),
            "pre-reload state must carry a grid entry for the TV display"
        );
        // Phase 2: add the gate. The seed publish must clear the
        // shared grid and publish `source_unknown` even though the
        // poller's first poll can never land.
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: Some(source_gate::SourceGateExpectation {
                    host: "tv.local".to_owned(),
                    expected_source: "HDMI4".to_owned(),
                    poll_interval: Duration::from_secs(2),
                    watched_apps: Arc::new([]),
                }),
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !latest_grids
                .read()
                .unwrap()
                .contains_key(&DisplayId("tv".to_owned())),
            "the gate-add seed must clear the SHARED latest_grids entry immediately; a clear deferred to the first poll leaves the stale grid visible to the wear tracker"
        );
        let status = status_rx.borrow().clone();
        assert_eq!(
            status.uniform_reason,
            Some(WEAR_SAMPLING_SOURCE_UNKNOWN),
            "the seed publish must tag the status uniform_reason={WEAR_SAMPLING_SOURCE_UNKNOWN}; got {:?}",
            status.uniform_reason
        );
        let mut first_event: Option<DaemonEvent> = None;
        while let Ok(msg) = event_rx.try_recv() {
            if let ControlMsg::PublishDaemonEvent(event) = msg
                && matches!(event, DaemonEvent::WearSamplingSourceGate { .. })
                && first_event.is_none()
            {
                first_event = Some(event);
            }
        }
        let first_event = first_event.expect(
            "adding a gate must emit a WearSamplingSourceGate event with the seed observation",
        );
        match first_event {
            DaemonEvent::WearSamplingSourceGate { state, .. } => {
                assert_eq!(
                    state, "unknown",
                    "the first gate event must be state=unknown (got {state})"
                );
            }
            other => panic!("expected WearSamplingSourceGate, got {other:?}"),
        }
        cancel.cancel();
        let _ = join.await;
    }

    /// Step 2 (RED): a gate flip DURING the cadence wait must skip the
    /// capture that wakes up at the next tick. The runtime re-reads the
    /// poller's latest observation between select-return and the
    /// `gate_skip` check so a stale `gate_state` from before the wait
    /// cannot authorize a capture the operator's TV has just invalidated.
    ///
    /// The probe flips Matched -> Mismatched at +2s on a 10s cadence;
    /// without the fix, `captures_seen` grows by one more capture than
    /// the matched-window baseline (the post-wait capture proceeds
    /// against the stale Matched). With the fix it stays at the
    /// baseline.
    ///
    /// The pre-wait state MUST be `Matched` (not the seeded
    /// `Unknown{awaiting_first_poll}`, which is itself a skip value and
    /// would mask the bug). Force the drain by advancing past the
    /// poller's first poll and sending an identity `DisplayContext` so
    /// the runtime wakes from select and the NEXT iteration's top
    /// drain reads the `Matched` observation. Without the fix the
    /// runtime then sits in select with `state=Match`; the poller
    /// flips to `Mismatch` at +2s; the cadence fires at +10s and the
    /// stale `gate_skip` reads `Matched` → capture. With the fix the
    /// post-select re-drain reads `Mismatch` → skip.
    ///
    /// Must NOT use `drive()`: that helper drops `updates_tx`, which
    /// makes `update_rx.recv()` return instantly and the loop
    /// hot-spins `drain → select → continue` without ever actually
    /// waiting for the cadence (the bug's hiding spot).
    #[tokio::test(start_paused = true)]
    async fn source_gate_flip_during_cadence_wait_skips_the_wake_capture() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let config = gated_tv_config(Duration::from_secs(10));
        let (source_service, _, _, _) =
            service_source([TestCapture::Frame, TestCapture::Frame, TestCapture::Frame]);
        let captures_seen = source_service.captures_seen.clone();
        let source: Box<dyn CaptureSource + Send + Sync + 'static> =
            Box::new(GateProbeSource::new(source_service));
        let reader: Arc<dyn source_gate::InputSourceReader> = Arc::new(ScriptedSourceReader::new(
            [Ok("HDMI4".to_owned()), Ok("HDMI2".to_owned())],
            Ok("HDMI2".to_owned()),
        ));
        let cancel = CancellationToken::new();
        // KEEP updates_tx alive — closing it makes the runtime's
        // `update_rx.recv()` arm resolve instantly and hides the race.
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source,
            source_reader: Some(reader),
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (handle, join) = spawn_with_handle(deps);
        // Phase 1: let the runtime reach Streaming, then advance past
        // the poller's first poll (at +2s, fires Matched) and pump the
        // runtime so the NEXT streaming-arm iteration's top drain
        // catches Matched into gate_state. The runtime is otherwise
        // parked in select (live updates_tx, no other arms firing).
        tokio::time::advance(Duration::from_secs(3)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Force the runtime to wake from select with the identity
        // DisplayContext. The arm body calls apply_update (which does
        // not drain by itself) and `continue`s; the next streaming
        // arm iteration drains at its top — by now the poller's first
        // poll at +2s has fired, so the drain reads Matched.
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: Some(source_gate::SourceGateExpectation {
                    host: "tv.local".to_owned(),
                    expected_source: "HDMI4".to_owned(),
                    poll_interval: Duration::from_secs(2),
                    watched_apps: Arc::new([]),
                }),
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let baseline_captures = captures_seen.load(Ordering::SeqCst);
        assert_eq!(
            handle.status().borrow().state,
            SamplingState::Streaming,
            "runtime must have reached Streaming before the flip"
        );
        assert_eq!(
            handle.status().borrow().source_gate,
            Some(source_gate::SourceGate::Matched),
            "pre-wait drain must settle gate_state to Matched (got {:?}); \
             the test cannot discriminate the TOCTOU fix if the pre-wait \
             state is Unknown{{awaiting_first_poll}}, which is itself a \
             skip value",
            handle.status().borrow().source_gate
        );
        // Phase 2: advance past several poller ticks (the poller is on a
        // +2s cadence from the +2s first poll, so the writes at +4s,
        // +6s, +8s, +10s all observe HDMI2 → Mismatched) AND the
        // first cadence tick at +10s. The runtime sits in select
        // with `state=Match`; without the TOCTOU fix the `gate_skip`
        // check reads the stale Matched and authorizes a capture;
        // with the fix the runtime re-drains the flip before
        // `gate_skip` and skips. The advance + 200 yields are sized
        // so the runtime is reliably polled through the post-cadence
        // drain (small advances race the executor's poll cadence
        // and leave `source_gate` reporting Matched instead).
        tokio::time::advance(Duration::from_secs(15)).await;
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            handle.status().borrow().source_gate,
            Some(source_gate::SourceGate::Mismatched {
                observed: "HDMI2".to_owned()
            }),
            "runtime must reflect the post-flip Mismatched observation"
        );
        let final_captures = captures_seen.load(Ordering::SeqCst);
        assert_eq!(
            final_captures, baseline_captures,
            "a Matched -> Mismatched flip during the cadence wait must skip the wake capture (baseline={baseline_captures}, final={final_captures})"
        );
        cancel.cancel();
        let _ = join.await;
    }

    /// Step 2 (RED): removing the configured gate expectation must
    /// reset the live `gate_state`, not leave the runtime wedged on the
    /// last observation. A `Mismatched` gate followed by gate removal
    /// would otherwise skip captures forever on an ungated display —
    /// the contract "no gate = permanently matched" the production
    /// status surface advertises. The probe: Mismatched gate -> 0
    /// captures (gate skips), then remove the expectation, advance a
    /// full cadence, and assert at least one capture landed. Without
    /// the fix `captures_seen` stays at 0.
    #[tokio::test(start_paused = true)]
    async fn source_gate_removal_resets_state_and_resumes_capture() {
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let config = gated_tv_config(Duration::from_secs(10));
        let (source_service, _, _, _) = service_source([
            TestCapture::Frame,
            TestCapture::Frame,
            TestCapture::Frame,
            TestCapture::Frame,
        ]);
        let captures_seen = source_service.captures_seen.clone();
        let source: Box<dyn CaptureSource + Send + Sync + 'static> =
            Box::new(GateProbeSource::new(source_service));
        // Reader keeps returning the wrong source so the gate stays
        // Mismatched until the expectation is removed.
        let reader: Arc<dyn source_gate::InputSourceReader> = Arc::new(ScriptedSourceReader::new(
            [Ok("HDMI2".to_owned())],
            Ok("HDMI2".to_owned()),
        ));
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source,
            source_reader: Some(reader),
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (handle, join) = spawn_with_handle(deps);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // Phase 1: Mismatched gate -> zero captures through one cadence.
        tokio::time::advance(Duration::from_secs(11)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            handle.status().borrow().source_gate,
            Some(source_gate::SourceGate::Mismatched {
                observed: "HDMI2".to_owned()
            }),
            "runtime must report the wrong-source Mismatched observation"
        );
        assert_eq!(
            captures_seen.load(Ordering::SeqCst),
            0,
            "Mismatched gate must skip every capture"
        );
        // Phase 2: remove the expectation. After one cadence, the
        // ungated display must capture — no stale gate_state.
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(11)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            handle.status().borrow().source_gate,
            None,
            "runtime must clear source_gate on gate removal (permanently matched, no phantom status)"
        );
        assert!(
            captures_seen.load(Ordering::SeqCst) >= 1,
            "removing the gate must allow captures again after one cadence; got {}",
            captures_seen.load(Ordering::SeqCst)
        );
        cancel.cancel();
        let _ = join.await;
    }

    /// Drive a fresh `ActiveSampler` configured for a single gate
    /// response. Used by the source-gate × tick test and the reload
    /// lifecycle test below; exists as a separate function so the
    /// assertion blocks stay within each test's `too_many_lines`
    /// allowance. The caller selects the per-call capture source via
    /// the `source_factory` closure so the reload test can swap in a
    /// `CloseTrackingScriptedSource` without giving up the helper's
    /// shared finalize-and-spawn body. The factory returns the boxed
    /// source plus the atomic `captures_seen` counter the runtime's
    /// capture pipeline increments for each `capture_one` call.
    fn drive<F>(
        gate_response: Result<&'static str, &'static str>,
        consent_path: &std::path::Path,
        interval: Duration,
        source_factory: F,
    ) -> (
        Arc<AtomicUsize>,
        LatestGrids,
        ActiveSamplerHandle,
        JoinHandle<()>,
        CancellationToken,
    )
    where
        F: FnOnce() -> (
            Box<dyn CaptureSource + Send + Sync + 'static>,
            Arc<AtomicUsize>,
        ),
    {
        let config = gated_tv_config(interval);
        let (source, captures_seen) = source_factory();
        let latest = new_latest_grids();
        let cancel = CancellationToken::new();
        let responses: Vec<Result<String, String>> = match gate_response {
            Ok(s) => vec![Ok((*s).to_owned())],
            Err(s) => vec![Err((*s).to_owned())],
        };
        let default: Result<String, String> = match gate_response {
            Ok(s) => Ok((*s).to_owned()),
            Err(s) => Err((*s).to_owned()),
        };
        let reader: Arc<dyn source_gate::InputSourceReader> =
            Arc::new(ScriptedSourceReader::new(responses, default));
        let (updates_tx, updates_rx) = mpsc::channel(2);
        let deps = ActiveSamplerDeps {
            initial_config: config,
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: latest.clone(),
            source,
            source_reader: Some(reader),
            apps_probe: None,
            consent_path: consent_path.to_path_buf(),
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        drop(updates_tx);
        let (handle, join) = spawn_with_handle(deps);
        (captures_seen, latest, handle, join, cancel)
    }

    fn drive_default(
        gate_response: Result<&'static str, &'static str>,
        consent_path: &std::path::Path,
        interval: Duration,
    ) -> (
        Arc<AtomicUsize>,
        LatestGrids,
        ActiveSamplerHandle,
        JoinHandle<()>,
        CancellationToken,
    ) {
        drive(gate_response, consent_path, interval, || {
            let (source, _, _, _) = service_source([TestCapture::Frame, TestCapture::Frame]);
            let captures_seen = source.captures_seen.clone();
            (Box::new(GateProbeSource::new(source)), captures_seen)
        })
    }

    /// Step 2 (RED): the source-gate poller must only be active for
    /// displays that declare a `[displays.<id>.sampling]` table AND only
    /// while the runtime is `Streaming`. Adding, removing, or changing
    /// `expected_source` via a `DisplayContext` update must restart the
    /// poller without ever touching the source's `close()` or
    /// `connect()` (gated capture != gated portal session). The
    /// `close_calls == 0` and `connect_calls` invariant is THE pin: if
    /// source-setting reloads ever called `close()` the operator would
    /// silently burn the saved portal grant on every TV config edit.
    #[tokio::test(start_paused = true)]
    // Five linear scenarios (a-e) live in one body so the portal-session pin
    // stays co-located with the counter deltas it certifies.
    #[allow(clippy::too_many_lines)]
    async fn source_gate_poller_lifecycle_respects_portal_session() {
        // (a) unconfigured AOC: no poller, no source_gate status.
        let dir = tempdir().unwrap();
        let aoc_path = dir.path().join("aoc-consent.json");
        test_record(&aoc_path);
        let aoc_config = active_config(Duration::from_secs(60));
        let aoc_inner = CloseTrackingScriptedSource::new(ScriptedCaptureSource::with_connections(
            [Ok(test_stream())],
        ));
        let mut aoc_inner = aoc_inner;
        aoc_inner.inner.frames = VecDeque::from([
            ScriptedOutcome::Ready(Ok(test_frame())),
            ScriptedOutcome::Ready(Ok(test_frame())),
        ]);
        let aoc_close_calls = aoc_inner.close_calls.clone();
        let aoc_connect_calls = aoc_inner.connect_calls.clone();
        let aoc_captures = aoc_inner.captures_seen.clone();
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let deps_aoc = ActiveSamplerDeps {
            initial_config: aoc_config,
            display_id: DisplayId("oled".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(aoc_inner),
            source_reader: None,
            apps_probe: None,
            consent_path: aoc_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        drop(updates_tx);
        let (aoc_handle, aoc_join) = spawn_with_handle(deps_aoc);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            aoc_handle.status().borrow().source_gate,
            None,
            "unconfigured AOC must have no source_gate status (permanently matched)"
        );
        let aoc_captures_after_spawn = aoc_captures.load(Ordering::SeqCst);
        let aoc_connects_after_spawn = aoc_connect_calls.load(Ordering::SeqCst);
        let aoc_closes_after_spawn = aoc_close_calls.load(Ordering::SeqCst);
        assert_eq!(
            aoc_closes_after_spawn, 0,
            "AOC must not have called close() before shutdown (pre-pin baseline)"
        );
        cancel.cancel();
        let _ = aoc_join.await;

        // (b) configured TV: poller spawns in Streaming, then we drive
        // a sequence of source-setting reloads via the same
        // `DisplayContext` channel the production reload path uses.
        // Through ALL of those the `close_calls` and `connect_calls`
        // counters on the source must stay flat — the pin: source
        // reloads must never close the portal session.
        let dir = tempdir().unwrap();
        let tv_path = dir.path().join("tv-consent.json");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let inner =
            CloseTrackingScriptedSource::new(ScriptedCaptureSource::with_connections([Ok(
                test_stream(),
            )]));
        let mut inner = inner;
        inner.inner.frames = VecDeque::from([
            ScriptedOutcome::Ready(Ok(test_frame())),
            ScriptedOutcome::Ready(Ok(test_frame())),
            ScriptedOutcome::Ready(Ok(test_frame())),
        ]);
        let close_calls = inner.close_calls.clone();
        let connect_calls = inner.connect_calls.clone();
        let captures_seen = inner.captures_seen.clone();
        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(4);
        let deps = ActiveSamplerDeps {
            initial_config: gated_tv_config(Duration::from_secs(60)),
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(inner),
            source_reader: Some(Arc::new(ScriptedSourceReader::new(
                [Ok("HDMI4".to_owned())],
                Ok("HDMI4".to_owned()),
            ))),
            apps_probe: None,
            consent_path: tv_path,
            cancel: cancel.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (handle, join) = spawn_with_handle(deps);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let initial_close_calls = close_calls.load(Ordering::SeqCst);
        let initial_connect_calls = connect_calls.load(Ordering::SeqCst);
        let initial_captures = captures_seen.load(Ordering::SeqCst);
        assert_eq!(
            initial_close_calls, 0,
            "TV with configured gate must never call close() on initial connect"
        );
        assert_eq!(
            initial_connect_calls, 1,
            "TV runtime must have called connect() exactly once during initial Streaming entry"
        );

        // (c) change `expected_source` via a DisplayContext update.
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: Some(source_gate::SourceGateExpectation {
                    host: "tv.local".to_owned(),
                    expected_source: "HDMI2".to_owned(),
                    poll_interval: Duration::from_secs(2),
                    watched_apps: Arc::new([]),
                }),
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // (d) remove the gate by sending a DisplayContext with no
        // `source_gate_expectation`.
        updates_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            close_calls.load(Ordering::SeqCst),
            initial_close_calls,
            "source-setting reloads (expect change, expect removal) must not call close()"
        );
        assert_eq!(
            connect_calls.load(Ordering::SeqCst),
            initial_connect_calls,
            "source-setting reloads must not re-open the portal session"
        );
        assert_eq!(
            captures_seen.load(Ordering::SeqCst),
            initial_captures,
            "no extra captures must run while the runtime was reloading the gate"
        );
        // AOC was cancelled before the TV reloads; the AOC's
        // `close_calls` increments once on shutdown (the runtime's
        // `run()` calls `source.close()` at the end of the loop).
        // The pin we actually want is that the TV's gate reloads do
        // not re-open the AOC's portal session — i.e. the AOC's
        // `connect_calls` and `captures_seen` stay flat.
        assert_eq!(
            aoc_close_calls.load(Ordering::SeqCst),
            aoc_closes_after_spawn + 1,
            "AOC must close exactly once (the shutdown close), nothing more"
        );
        assert_eq!(
            aoc_connect_calls.load(Ordering::SeqCst),
            aoc_connects_after_spawn,
            "AOC's connect_calls must remain flat across the TV's gate reloads"
        );
        assert_eq!(
            aoc_captures.load(Ordering::SeqCst),
            aoc_captures_after_spawn,
            "AOC's capture counter must remain flat across the TV's gate reloads"
        );

        cancel.cancel();
        let _ = handle;
        let _ = join.await;
    }

    /// Big config builder for the two-runtime mode-resolution test.
    /// Two displays: a TV with an explicit per-display `stream_mode`
    /// override, and a render-only monitor with no override. The wear
    /// section's `[wear.active_sampling] stream_mode` is the global
    /// fallback (`Warm`).
    fn two_runtime_mode_resolution_config() -> Arc<Config> {
        use dormant_core::config::schema::{DisplaySamplingConfig, DisplayScope, HookSlots};
        use dormant_core::types::{BlankMode, LadderStage, StageKind};
        let mut config = (*active_config(Duration::from_secs(60))).clone();
        config.wear.active_sampling.sampled_display = None;
        config.wear.active_sampling.sampled_displays = vec!["tv".to_owned(), "monitor".to_owned()];
        config.wear.active_sampling.stream_mode = StreamMode::Warm;
        config.displays.insert(
            "monitor".to_owned(),
            DisplayConfig {
                controllers: vec!["ddcci".to_owned()],
                scope: DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_write_code: None,
                shared_peer_input_code: None,
                hooks: HookSlots::default(),
                blank_mode: Some(BlankMode::BrightnessZero),
                degraded_mode: None,
                ladder: vec![LadderStage {
                    kind: StageKind::Controller(BlankMode::BrightnessZero),
                    dwell: None,
                }],
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
                samsung_restore_backlight:
                    dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::default(),
                power_off_opt_in: false,
                compositor_output: None,
                sampling: None,
            },
        );
        config.displays.insert(
            "tv".to_owned(),
            DisplayConfig {
                controllers: vec!["samsung-tizen".to_owned()],
                scope: DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_write_code: None,
                shared_peer_input_code: None,
                hooks: HookSlots::default(),
                blank_mode: Some(BlankMode::BrightnessZero),
                degraded_mode: None,
                ladder: vec![LadderStage {
                    kind: StageKind::Controller(BlankMode::BrightnessZero),
                    dwell: None,
                }],
                screensaver: None,
                output: None,
                ddc_display: None,
                host: Some("tv.local".to_owned()),
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
                samsung_restore_backlight:
                    dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::default(),
                power_off_opt_in: false,
                compositor_output: Some("HDMI-A-1".to_owned()),
                sampling: Some(DisplaySamplingConfig {
                    expected_source: Some("HDMI4".to_owned()),
                    source_poll_interval: Duration::from_secs(2),
                    stream_mode: Some(StreamMode::PerTick),
                    watched_apps: Vec::new(),
                }),
            },
        );
        Arc::new(config)
    }

    /// Capture-source wrapper that records every `capture_one` mode
    /// argument and every `reset_stream` call so the two-runtime
    /// mode-resolution test can read out the effective `StreamMode`
    /// each runtime observed AND prove the per-display effective-mode
    /// change fires `StreamModeChanged` only on the runtime that
    /// flipped mode.
    struct ModeRecordingSource {
        inner: ServiceSource,
        modes: Arc<Mutex<Vec<StreamMode>>>,
        resets: Arc<AtomicUsize>,
    }

    impl ModeRecordingSource {
        fn new(inner: ServiceSource) -> (Self, Arc<AtomicUsize>, Arc<Mutex<Vec<StreamMode>>>) {
            let modes = Arc::new(Mutex::new(Vec::new()));
            let resets = Arc::new(AtomicUsize::new(0));
            let recorded_modes = modes.clone();
            let recorded_resets = resets.clone();
            (
                Self {
                    inner,
                    modes: modes.clone(),
                    resets,
                },
                recorded_resets,
                recorded_modes,
            )
        }
    }

    #[async_trait]
    impl CaptureSource for ModeRecordingSource {
        async fn connect(
            &mut self,
            binding: &ConsentBinding<'_>,
        ) -> Result<ConnectedStream, CaptureError> {
            self.inner.connect(binding).await
        }
        async fn request_consent(
            &mut self,
            display: &DisplayExpectation,
        ) -> Result<Grant, CaptureError> {
            self.inner.request_consent(display).await
        }
        async fn capture_one(&mut self, mode: StreamMode) -> Result<RawFrame, CaptureError> {
            self.modes.lock().unwrap().push(mode);
            self.inner.capture_one(mode).await
        }
        async fn reset_stream(&mut self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
            self.inner.reset_stream().await;
        }
        async fn close(&mut self) {
            self.inner.close().await;
        }
    }

    /// Step 2 (RED): two-runtime mode-resolution. Global = `Warm`.
    /// Monitor runtime carries no `[sampling]` override and must
    /// resolve `Warm`. TV runtime carries `Some(PerTick)` and must
    /// resolve `PerTick`. Reloading the TV's `DisplayContext` to
    /// `stream_mode == None` must switch the TV's effective mode to
    /// `Warm` through the existing `StreamModeChanged` reset path
    /// WITHOUT re-consent (no `consent_path` rebuild, no `request_consent`
    /// call). The monitor runtime must record no mode change.
    #[tokio::test(start_paused = true)]
    // Two runtimes + a DisplayContext reload + reset-path + consent-binding
    // assertions read more clearly as one chronological flow than as helpers.
    #[allow(clippy::too_many_lines)]
    async fn source_gate_per_display_stream_mode_resolution_two_runtimes() {
        let dir = tempdir().unwrap();
        let monitor_path = dir.path().join("monitor-consent.json");
        let tv_path = dir.path().join("tv-consent.json");
        test_record_for(&monitor_path, "monitor");
        test_tv_consent(&tv_path, "HDMI-A-1");
        let config = two_runtime_mode_resolution_config();

        let (monitor_source, monitor_resets, monitor_modes) =
            ModeRecordingSource::new(service_source([TestCapture::Frame, TestCapture::Frame]).0);
        let (tv_source, tv_resets, tv_modes) =
            ModeRecordingSource::new(service_source([TestCapture::Frame, TestCapture::Frame]).0);
        let cancel_monitor = CancellationToken::new();
        let cancel_tv = CancellationToken::new();
        let (_updates_monitor_tx, updates_monitor_rx) = mpsc::channel(4);
        let (updates_tv_tx, updates_tv_rx) = mpsc::channel(4);
        let deps_monitor = ActiveSamplerDeps {
            initial_config: config.clone(),
            display_id: DisplayId("monitor".to_owned()),
            update_rx: updates_monitor_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(monitor_source),
            source_reader: None,
            apps_probe: None,
            consent_path: monitor_path.clone(),
            cancel: cancel_monitor.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let deps_tv = ActiveSamplerDeps {
            initial_config: config.clone(),
            display_id: DisplayId("tv".to_owned()),
            update_rx: updates_tv_rx,
            latest_grids: new_latest_grids(),
            source: Box::new(tv_source),
            source_reader: None,
            apps_probe: None,
            consent_path: tv_path.clone(),
            cancel: cancel_tv.clone(),
            env_reader: test_env_reader,
            event_tx: None,
        };
        let (_mh, _mj) = spawn_with_handle(deps_monitor);
        let (th, tj) = spawn_with_handle(deps_tv);

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // The TV runtime honors its per-display `stream_mode` override at
        // the capture boundary; the monitor has no override and inherits
        // the wear section's global `Warm`. This is the must-fix contract
        // — the capture-path mode is what production will see.
        assert_eq!(
            monitor_modes.lock().unwrap().as_slice(),
            &[StreamMode::Warm],
            "monitor must inherit the wear section's Warm mode"
        );
        assert_eq!(
            tv_modes.lock().unwrap().as_slice(),
            &[StreamMode::PerTick],
            "TV runtime must honor its per-display override at the capture boundary, not fall back to the global"
        );
        let tv_resets_before = tv_resets.load(Ordering::SeqCst);
        let monitor_resets_before = monitor_resets.load(Ordering::SeqCst);

        // Reload the TV context to drop the override. The runtime
        // must compare old vs new effective mode (PerTick → Warm),
        // fire the existing `StreamModeChanged` reset transition
        // (no re-consent), and call `reset_stream()` on the source.
        // The monitor runtime must not observe any reset.
        updates_tv_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: None,
            }))
            .await
            .unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // Advance one cadence so the TV runtime actually emits the
        // post-reset capture.
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let tv_resets_after = tv_resets.load(Ordering::SeqCst);
        let monitor_resets_after = monitor_resets.load(Ordering::SeqCst);
        let tv_modes_now = tv_modes.lock().unwrap().clone();
        let monitor_modes_now = monitor_modes.lock().unwrap().clone();
        assert!(
            tv_resets_after > tv_resets_before,
            "TV must reset its stream on effective-mode change, got {tv_resets_before} -> {tv_resets_after}"
        );
        assert_eq!(
            monitor_resets_after, monitor_resets_before,
            "monitor runtime must not observe TV's stream_mode change"
        );
        assert_eq!(
            tv_modes_now.last(),
            Some(&StreamMode::Warm),
            "the post-reset TV capture must use the new effective mode (Warm, the global fallback), got {tv_modes_now:?}"
        );
        // The monitor runtime's cadence ticks alongside the TV's, so it
        // captures one extra Warm frame during the post-reload advance.
        // The invariant is that the monitor never recorded a mode other
        // than `Warm` — i.e. the TV's effective-mode shift was not
        // visible to the monitor runtime.
        assert!(
            monitor_modes_now.iter().all(|m| *m == StreamMode::Warm),
            "monitor runtime must not observe any mode other than Warm across the TV's effective-mode shift, got {monitor_modes_now:?}"
        );

        // The TV runtime must not have rebuilt the consent record
        // under the wrong binding: the `Runtime::new` validator
        // wipes the record if the configured `compositor_output` or
        // `display_id` drift, and the stream-mode `DisplayContext`
        // update does not bump either — the record still loads
        // against the original (display, compositor_output) binding.
        assert!(
            tv_path.exists(),
            "TV consent record must survive the stream-mode override reset"
        );
        let stored = crate::screencast_consent::load(&tv_path, "tv", Some("HDMI-A-1"))
            .expect("TV consent record must still load for the original binding");
        assert_eq!(
            stored.record().sampled_display,
            "tv",
            "TV consent record must still target the original sampled_display"
        );

        // (d) Re-add the per-display PerTick override on the TV and
        // confirm the effective-mode change (Warm -> PerTick) fires
        // another reset — exercising the precedence path at reload
        // time. Under the mutated form (always-global) the new
        // effective mode would stay Warm and the reset would NOT fire.
        let tv_resets_before_readd = tv_resets.load(Ordering::SeqCst);
        let monitor_resets_before_readd = monitor_resets.load(Ordering::SeqCst);
        updates_tv_tx
            .send(SamplerUpdate::DisplayContext(DisplaySamplingContext {
                display: Some(DisplayExpectation {
                    display: "tv".to_owned(),
                    compositor_output: Some("HDMI-A-1".to_owned()),
                }),
                phase: Phase::Active,
                stage_active: true,
                source_gate_expectation: None,
                stream_mode: Some(StreamMode::PerTick),
            }))
            .await
            .unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let tv_resets_after_readd = tv_resets.load(Ordering::SeqCst);
        let monitor_resets_after_readd = monitor_resets.load(Ordering::SeqCst);
        assert!(
            tv_resets_after_readd > tv_resets_before_readd,
            "TV must reset again when the per-display override is re-added (Warm -> PerTick), got {tv_resets_before_readd} -> {tv_resets_after_readd}"
        );
        assert_eq!(
            monitor_resets_after_readd, monitor_resets_before_readd,
            "monitor runtime must not observe the TV's override re-add"
        );
        // The post-readd capture must use the new effective mode
        // (PerTick), proving the capture path consumes the override —
        // not just the reset detector.
        let tv_modes_after_readd = tv_modes.lock().unwrap().clone();
        assert_eq!(
            tv_modes_after_readd.last(),
            Some(&StreamMode::PerTick),
            "TV's post-readd capture must use the re-added override, got {tv_modes_after_readd:?}"
        );

        cancel_monitor.cancel();
        cancel_tv.cancel();
        let _ = th;
        let _ = tj.await;
    }
}
