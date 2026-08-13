//! Async rules engine — wires sensor events through the zone engine into
//! per-display state machines, dispatches blank/wake commands on the I/O
//! executor side without blocking the event loop, and emits [`DaemonEvent`]s
//! for downstream consumers (`CLI`, `WebUI`, logs).
//!
//! ## Module layout
//!
//! - [`ControlMsg`] / [`DaemonEvent`] / [`StateSnapshot`] / friends —
//!   the I/O surfaces.
//! - [`RulesEngineConfig`] / [`DisplayRuntimeCfg`] / [`RuleRuntimeCfg`] /
//!   [`SensorRuntimeCfg`] — the per-runtime configuration shapes.
//! - [`RulesEngine`] — the engine itself.  Built by [`RulesEngine::new`],
//!   driven by [`RulesEngine::run`].
//!
//! ## Engine loop
//!
//! [`RulesEngine::run`] is a single `tokio::select!` over:
//!
//! - the presence-event mpsc (from sensor sources),
//! - the control mpsc (from the daemon / `IPC` / `WebUI`),
//! - the internal results mpsc (sink responses from spawned dispatch tasks),
//! - the timer wheel (display-machine ticks and sensor hold-expiry timers),
//! - a periodic stale-sensor sweeper.
//!
//! Sink calls are non-blocking — every
//! [`crate::state_machine::Effect::IssueBlank`] / `IssueWake` is handed to a
//! `tokio::spawn`ed task that clones the [`crate::traits::CommandSink`]
//! handle, awaits the call, and forwards the result back through the internal
//! mpsc.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::{DisplayScope, SensorKind};
use crate::coordination::CoordinationHandle;
use crate::error::DormantError;
use crate::observation::{DaemonObservation, GenerationId, ObservationHub};
use crate::ownership::OwnershipGate;
use crate::state_machine::{DisplayStateMachine, Effect, Input, SmTimings};
use crate::traits::{CommandSink, PanelState, PowerState, RenderSink};
use crate::types::{
    BlankMode, CmdFailure, DisplayId, LadderStage, PresenceEvent, RuleId, SensorId, SensorState,
    StageKind, Tick, Timestamp, ZoneId,
};
use crate::zone::ZoneEngine;

// ── Inhibitor kinds ─────────────────────────────────────────────────────────

/// The category of an inhibitor source that can suppress blanking for a rule.
///
/// Multiple inhibitor sources (user activity, audio playback, an active
/// call) can be engaged concurrently for the same rule; the engine tracks
/// each kind independently and OR-derives one effective inhibition bit from
/// them (see [`ControlMsg::SetInhibited`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InhibitorKind {
    /// User keyboard/mouse/idle-time activity (config literal
    /// [`INHIBITOR_USER_ACTIVITY`]).
    UserActivity,
    /// Active audio playback (config literal [`INHIBITOR_AUDIO_PLAYBACK`]).
    AudioPlayback,
    /// An active call (config literal [`INHIBITOR_CALL`]).
    Call,
}

/// Config-file literal for [`InhibitorKind::UserActivity`].
pub const INHIBITOR_USER_ACTIVITY: &str = "user-activity";
/// Config-file literal for [`InhibitorKind::AudioPlayback`].
pub const INHIBITOR_AUDIO_PLAYBACK: &str = "audio-playback";
/// Config-file literal for [`InhibitorKind::Call`].
pub const INHIBITOR_CALL: &str = "call";

impl InhibitorKind {
    /// Parse a config-file inhibitor literal into its [`InhibitorKind`].
    ///
    /// Returns `None` for any string that isn't one of the three known
    /// literals. Rejecting unknown inhibitor names at config-validation time
    /// is a config-layer concern, not this parser's.
    #[must_use]
    pub fn from_config(s: &str) -> Option<Self> {
        match s {
            INHIBITOR_USER_ACTIVITY => Some(Self::UserActivity),
            INHIBITOR_AUDIO_PLAYBACK => Some(Self::AudioPlayback),
            INHIBITOR_CALL => Some(Self::Call),
            _ => None,
        }
    }
}

// ── Public I/O surfaces ───────────────────────────────────────────────────────

/// The kind of hardware operation fenced by a daemon generation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperationKind {
    /// A single-display control-path exercise.
    Exercise(DisplayId),
    /// A global emergency wake.
    EmergencyWake,
}

/// The immutable identity accepted by an engine generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedOperation {
    /// Daemon-lifetime operation identity.
    pub id: u64,
    /// Generation that accepted the operation.
    pub generation: GenerationId,
    /// Hardware operation being performed.
    pub kind: OperationKind,
}

#[derive(Default)]
struct OperationState {
    next_id: u64,
    active: HashMap<u64, (GenerationId, OperationKind, CancellationToken)>,
    quiescing: HashSet<GenerationId>,
}

/// Daemon-lifetime active operation leases, shared by all generations.
#[derive(Clone, Default)]
pub struct OperationRegistry {
    state: Arc<Mutex<OperationState>>,
    changed: Arc<Notify>,
}

/// RAII ownership of one accepted hardware operation.
pub struct OperationLease {
    registry: OperationRegistry,
    id: u64,
    token: CancellationToken,
}

impl std::fmt::Debug for OperationLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationLease")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl OperationLease {
    /// Returns whether cancellation was requested for this operation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.registry.state.lock() {
            state.active.remove(&self.id);
        }
        self.registry.changed.notify_waiters();
    }
}

impl OperationRegistry {
    /// Close acceptance for a generation before quiescing its active leases.
    pub fn begin_quiesce(&self, generation: GenerationId) {
        if let Ok(mut state) = self.state.lock() {
            state.quiescing.insert(generation);
        }
    }

    /// Reopen operation acceptance after a rejected reload.
    pub fn end_quiesce(&self, generation: GenerationId) {
        if let Ok(mut state) = self.state.lock() {
            state.quiescing.remove(&generation);
        }
    }

    /// Adopt an operation accepted by the daemon front door.
    #[must_use]
    pub fn lease_accepted(&self, operation: &AcceptedOperation) -> Option<OperationLease> {
        let state = self.state.lock().ok()?;
        let (_, kind, token) = state.active.get(&operation.id)?;
        if operation.generation != state.active.get(&operation.id)?.0 || *kind != operation.kind {
            return None;
        }
        Some(OperationLease {
            registry: self.clone(),
            id: operation.id,
            token: token.clone(),
        })
    }

    /// Accept an operation unless a conflicting operation is active.
    ///
    /// # Errors
    ///
    /// Returns the requested kind when a conflicting operation is active.
    ///
    /// # Panics
    ///
    /// Panics if another thread poisoned the registry mutex.
    pub fn try_acquire(
        &self,
        generation: GenerationId,
        kind: OperationKind,
    ) -> Result<(AcceptedOperation, OperationLease), OperationKind> {
        let mut state = self
            .state
            .lock()
            .expect("operation registry mutex poisoned");
        if state.quiescing.contains(&generation) {
            return Err(kind);
        }
        let conflicts = state
            .active
            .values()
            .any(|(active_generation, active_kind, _)| {
                *active_generation == generation
                    && match (&kind, active_kind) {
                        (OperationKind::Exercise(left), OperationKind::Exercise(right)) => {
                            left == right
                        }
                        (
                            OperationKind::EmergencyWake | OperationKind::Exercise(_),
                            OperationKind::EmergencyWake,
                        )
                        | (OperationKind::EmergencyWake, OperationKind::Exercise(_)) => true,
                    }
            });
        if conflicts {
            return Err(kind);
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let token = CancellationToken::new();
        state
            .active
            .insert(id, (generation, kind.clone(), token.clone()));
        Ok((
            AcceptedOperation {
                id,
                generation,
                kind,
            },
            OperationLease {
                registry: self.clone(),
                id,
                token,
            },
        ))
    }

    /// Cancel all operations accepted by `generation`.
    pub fn cancel_generation(&self, generation: GenerationId) {
        if let Ok(state) = self.state.lock() {
            for (active_generation, _, token) in state.active.values() {
                if *active_generation == generation {
                    token.cancel();
                }
            }
        }
    }

    /// Wait until every operation accepted by `generation` has been dropped.
    pub async fn wait_generation_empty(&self, generation: GenerationId) {
        loop {
            let notified = self.changed.notified();
            let empty = self.state.lock().map_or(true, |state| {
                !state
                    .active
                    .values()
                    .any(|(active_generation, _, _)| *active_generation == generation)
            });
            if empty {
                return;
            }
            notified.await;
        }
    }
}

fn operation_completion_matches_generation(accepted: GenerationId, current: GenerationId) -> bool {
    accepted == current
}

/// Inbound control messages to the engine.
#[derive(Debug)]
pub enum ControlMsg {
    /// Pause blanking (wake path unaffected).  `rule: None` pauses every
    /// rule; `until: None` is an indefinite pause.
    Pause {
        /// Target rule (`None` → all rules).
        rule: Option<RuleId>,
        /// Auto-resume wall-clock deadline (`None` → indefinite).
        until: Option<Timestamp>,
    },
    /// Resume blanking (wake path is unaffected either way).
    Resume {
        /// Target rule (`None` → all rules).
        rule: Option<RuleId>,
    },
    /// Force-immediate blank (operator override).
    ForceBlank(DisplayId),
    /// Force-immediate wake (operator override).
    ForceWake(DisplayId),
    /// Walk the configured render/stage/controller blank ladder from its
    /// first stage (issue #124 — soft alternative to [`Self::ForceBlank`]
    /// that never hard-powers the panel).  Operator-initiated like
    /// `ForceBlank` but routes through [`Input::SoftBlank`] so the
    /// state machine enters the ladder instead of issuing a primary
    /// hardware blank.  When the ladder has no render stage this still
    /// issues a controller blank, but always at the configured
    /// `primary_blank_mode` rather than the operator override.
    SoftBlank(DisplayId),
    /// Request a current snapshot of engine state.
    Snapshot(oneshot::Sender<StateSnapshot>),
    /// Subscribe to [`DaemonEvent`]s from this point forward.
    SubscribeEvents(oneshot::Sender<broadcast::Receiver<DaemonEvent>>),
    /// Set the user-activity inhibitor state for a rule (or every rule).
    ///
    /// Routes [`Input::InhibitorChanged`] to the target rule's displays,
    /// mirroring the [`ControlMsg::Pause`] fan-out. The daemon's activity
    /// inhibitor publishes rule-level inhibition through this message.
    SetInhibited {
        /// Target rule (`None` → all rules).
        rule: Option<RuleId>,
        /// Which inhibitor category this update is for.
        kind: InhibitorKind,
        /// Whether the inhibitor is now engaged.
        inhibited: bool,
    },
    /// Set or clear the pending-reload indicator at runtime (operator feedback
    /// in [`StateSnapshot`]s). Lets the daemon flag a rejected reload without
    /// tearing down the running engine.
    SetPendingReload(Option<String>),
    /// Set or clear boot-time rollback metadata in snapshots.
    SetRollback(Option<RollbackStatus>),
    /// Set or clear the F10-claim-suppression deadline for `display`.
    /// `Some(deadline)` lifts the ownership-loss reaction for the
    /// `display` until that `Instant`; `None` clears it
    /// immediately. Owned and consulted by the rules engine;
    /// driven by the claim runtime in `dormantd`.
    SetClaimSuppression {
        /// Target display.
        display: DisplayId,
        /// Suppression deadline (`None` = clear).
        until: Option<Instant>,
    },
    /// Replace the [`KvmStatus`] the rules engine publishes in
    /// every [`StateSnapshot`]. Driven by the orchestrator on
    /// every successful generation install (fresh and reload)
    /// from the post-probe claim-capable display set.
    SetKvmStatus(KvmStatus),
    /// Input-wake event from the render surface — route to the display
    /// machine's [`Input::InputWake`].
    InputWake(DisplayId),
    /// Re-consult ownership for one display without waiting for a timer sweep.
    OwnershipPoll {
        /// Display whose ownership verdict must be refreshed.
        display: DisplayId,
    },
    /// Publish a daemon-side event onto this generation's event bus.
    /// Debug-asserts the event is not [`DaemonEvent::Unknown`] — the daemon
    /// never constructs that variant; it exists purely for forward-compat
    /// deserialization of events an OLDER client doesn't recognize yet.
    /// Passive — no engine state change beyond the broadcast send.
    PublishDaemonEvent(DaemonEvent),
    /// Force-wake EVERY display and pause every rule indefinitely, regardless
    /// of the per-display state machine's current phase.  Used by
    /// `dormantctl emergency-wake` as a one-shot panic-recovery command.
    ///
    /// The handler bypasses the normal `ForceWake` (per-display) flow so a
    /// wedged state machine (deadlocked in `Blanked` after a wake-retry
    /// storm) does not prevent the wake from going out — it calls
    /// [`CommandSink::wake_once`] directly on every display's executor,
    /// then forwards a [`Self::Pause`] with no `rule` and no `until` so the
    /// engine does not blank anything until the operator resumes.
    ///
    /// The reply carries a point-in-time view of what actually happened
    /// (per-display ok/err) so callers see partial-failure detail rather
    /// than a binary success bit.
    EmergencyWake {
        /// One-shot reply channel for the emergency report.
        reply: oneshot::Sender<EmergencyWakeReport>,
    },
    /// Generation-accepted emergency wake operation.
    EmergencyWakeAccepted {
        /// Lease identity accepted by the daemon registry.
        operation: AcceptedOperation,
        /// One-shot reply channel for the emergency report.
        reply: oneshot::Sender<EmergencyWakeReport>,
    },
    /// Run a control-path verification on a single display — blank, read,
    /// wake, read, restore — and return a per-step report.
    ///
    /// Used by `dormantctl doctor --exercise <display>` to confirm that a
    /// blank/wake command actually moved the panel, not just that the
    /// controller returned `Ok`.  The handler pauses every rule that drives
    /// the target display for the exercise window (so a presence edge cannot
    /// race the test commands), runs the exercise sequence on the
    /// display's executor, restores the pre-exercise phase, and un-pauses
    /// the rules.  The reply carries an [`ExerciseReport`] with one
    /// [`ExerciseStep`] per phase and a per-step
    /// [`ExerciseVerdict`] (`Confirmed` / `Unconfirmable` / `Failed`).
    ///
    /// The wake path is sacred: the restore step guarantees a final wake
    /// regardless of any earlier failure, so an exercise can never leave a
    /// display dark.
    Exercise {
        /// The display to exercise.
        display: DisplayId,
        /// One-shot reply channel for the exercise report.
        reply: oneshot::Sender<ExerciseReport>,
    },
    /// Generation-accepted display exercise operation.
    ExerciseAccepted {
        /// Lease identity accepted by the daemon registry.
        operation: AcceptedOperation,
        /// One-shot reply channel for the exercise report.
        reply: oneshot::Sender<ExerciseReport>,
    },
    /// Internal generation-swap fence. The daemon sends this only after all
    /// generation-local producers have stopped, so acknowledgement proves the
    /// old engine consumed every input queued before the swap.
    GenerationBarrier(oneshot::Sender<()>),
    /// An availability (LWT) edge from a sensor source. `online = true`
    /// records the source's reachability verdict in
    /// `RulesEngine::availability_online`, which the stale sweep consults
    /// to decide whether topic silence may be treated as a stale timeout.
    /// `online = false` (LWT) clears the assertion and synthesises an
    /// [`crate::types::SensorState::Unavailable`] presence edge so the
    /// fail-safe path takes over immediately.
    SensorAvailability(crate::types::SensorAvailabilityEvent),
}

/// Per-display outcome of an [`ControlMsg::EmergencyWake`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmergencyWakeResult {
    /// The display this result applies to.
    pub display: DisplayId,
    /// Whether [`CommandSink::wake_once`] returned `Ok`.
    pub ok: bool,
    /// Failure detail (controller + error) when `ok` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated report returned by an [`ControlMsg::EmergencyWake`] handler.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmergencyWakeReport {
    /// Whether every rule was paused.  `true` on success; `false` if the
    /// global pause fan-out encountered an error (very rare — rules set
    /// + `Input::Pause` step per display, so this is mostly diagnostic).
    pub paused: bool,
    /// Accepted operation identity, when the request was generation-fenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Generation that accepted the operation, when fenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationId>,
    /// Per-display wake results, one entry per display the engine owns.
    pub displays: Vec<EmergencyWakeResult>,
}

/// Verdict for a single step in a [`ControlMsg::Exercise`] sequence.
///
/// `Unconfirmable` and `Confirmed` are exit-zero verdicts for the CLI
/// (`dormantctl doctor --exercise` returns 0); `Failed` is the
/// exit-non-zero verdict — a panel that the controller can read but that
/// did not move in response to the test command.  That is the exact
/// failure shape `doctor --exercise` exists to surface (a controller that
/// logged `Ok` while the panel never changed), so the CLI maps it to a
/// non-zero exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExerciseVerdict {
    /// The controller reported the panel moved in the expected direction
    /// (blank step: state changed from baseline; wake step: state returned
    /// to baseline).
    Confirmed,
    /// The controller has no readback for this step — the command was
    /// issued but the panel could not be observed.  Exit 0 (honest, not
    /// a fake pass).
    Unconfirmable,
    /// The controller can read the panel but the state did NOT move as
    /// expected — the command returned `Ok` but the panel did not change.
    /// Exit non-zero.
    Failed,
}

/// One phase of a [`ControlMsg::Exercise`] sequence: the command issued, the
/// pre/post [`PanelState`] snapshot, and the [`ExerciseVerdict`] the engine
/// derived from the comparison.
///
/// The wire form carries a small, grep-stable `command` string
/// (`"blank"`, `"wake"`, `"read"`, `"restore"`) rather than the full blank
/// mode so the CLI can render it without re-deriving the mode from the
/// display's runtime config.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExerciseStep {
    /// Stable verb describing this step (`"blank"`, `"wake"`, `"restore"`,
    /// `"read"`).
    pub command: String,
    /// The blank mode that was used for the `blank` step (when applicable);
    /// `None` for `wake`, `read`, and `restore` steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blank_mode: Option<BlankMode>,
    /// Whether the controller's command call returned `Ok`.  Even a
    /// `returned_ok == true` step can be `Failed` if the panel-state
    /// comparison disagrees — that is the whole point of this report.
    pub returned_ok: bool,
    /// Panel state observed before the command (when a read was possible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_before: Option<PanelState>,
    /// Panel state observed after the command (when a read was possible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_after: Option<PanelState>,
    /// Verdict for this step.
    pub verdict: ExerciseVerdict,
    /// Optional error detail (controller + error string) for the `Ok`-but-
    /// not-really case or for the read that failed before the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated report returned by [`ControlMsg::Exercise`].
///
/// The CLI maps this to per-step ✓ / ~ / ✗ glyphs and exits non-zero if any
/// step verdict is `Failed`.  `paused_rules` carries the literal rule ids
/// the handler paused for the exercise window.  The pause release is
/// guaranteed engine-side via the internal `ExerciseResume` result — the
/// field here is informational so the CLI can show which rules were
/// affected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExerciseReport {
    /// The display the exercise ran on.
    pub display: DisplayId,
    /// The phase the display was in before the exercise started (so the
    /// operator can confirm the restore target was the right one).
    pub pre_phase: String,
    /// Accepted operation identity, when the request was generation-fenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Generation that accepted the operation, when fenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationId>,
    /// Rule ids the handler paused for the exercise window.  Empty for
    /// manual-only displays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paused_rules: Vec<RuleId>,
    /// Per-step outcomes.
    pub steps: Vec<ExerciseStep>,
}

/// Outbound events emitted by the engine for downstream consumers.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// A sensor's state changed.
    SensorChanged {
        /// The sensor whose state changed.
        sensor: SensorId,
        /// The new state.
        state: SensorState,
    },
    /// A zone's resolved presence flipped.
    ZoneChanged {
        /// The zone whose presence changed.
        zone: ZoneId,
        /// The new resolved presence.
        present: bool,
        /// The sensor whose event triggered the flip.
        cause: SensorId,
    },
    /// A display transitioned between phases.
    DisplayPhase {
        /// The display that transitioned.
        display: DisplayId,
        /// The literal name of the destination phase (grep-stable).
        phase: String,
        /// The literal cause of the transition.
        cause: String,
    },
    /// Manual or scheduled pause state changed for a display.
    PauseChanged {
        /// The display whose pause state changed.
        display: DisplayId,
        /// Whether blanking is currently paused.
        paused: bool,
        /// Rule scope of the pause; `None` denotes a global pause.
        rule: Option<RuleId>,
    },
    /// Configuration has been (re)loaded.
    ConfigReloaded,
    /// A wake command failed and a retry was scheduled.
    WakeRetry {
        /// The display whose wake failed.
        display: DisplayId,
        /// Monotonically increasing retry attempt counter (per display).
        attempt: u64,
    },
    /// A wear-tracking sample was taken for a display.  `display` is
    /// required (a wear event without its display is meaningless);
    /// `total_on_hours` / `sample_count` are `#[serde(default)]` so a
    /// future producer can omit them without breaking older consumers.
    WearSnapshot {
        /// The display this snapshot applies to.
        display: DisplayId,
        /// Cumulative on-hours tracked for this display.
        #[serde(default)]
        total_on_hours: f64,
        /// Number of wear samples folded into `total_on_hours` so far.
        #[serde(default)]
        sample_count: u64,
        /// Attribution method used for this observation.
        #[serde(default)]
        wear_attribution_mode: crate::wear::WearAttributionMode,
    },
    /// Active wear sampling entered its streaming lifecycle state.
    WearSamplingStarted,
    /// Active wear sampling entered a degraded uniform-attribution episode.
    WearSamplingDegraded {
        /// Stable reason for the degradation episode.
        reason: String,
    },
    /// Source-gate observation for a sampled display changed to a new full
    /// value (`matched` | `mismatched` | `unknown`). Steady-state polls do
    /// NOT re-fire this event — the runtime deduplicates by full `SourceGate`
    /// value so a long sequence of mismatched polls emits one event, not
    /// N. `observed` carries the polled source label when the TV reported one;
    /// `None` for matched and unknown gates.
    WearSamplingSourceGate {
        /// Display the gate is bound to.
        display: DisplayId,
        /// One of `matched`, `mismatched`, `unknown`.
        state: String,
        /// Observed source label for mismatched polls; `None` otherwise.
        #[serde(default)]
        observed: Option<String>,
    },
    /// Advisory nudge: the display has gone this many hours since its last
    /// long-dwell static-content window (a hint the WebUI/CLI can use to
    /// suggest compensation, e.g. a pixel-shift or a brightness nudge).
    /// `display` is required; `hours_since_long_dwell` is
    /// `#[serde(default)]` for forward compat.
    CompensationAdvisory {
        /// The display this advisory applies to.
        display: DisplayId,
        /// Hours elapsed since the display's last long-dwell window.
        #[serde(default)]
        hours_since_long_dwell: u64,
    },
    /// A blank command exhausted its controller chain (every controller in
    /// the ladder failed).  `display` is required; `controller` /
    /// `detail` are `#[serde(default)]` so a future producer can omit them
    /// without breaking older consumers.  NOTE: `blank_failure` is the wire
    /// tag (via `rename_all = "snake_case"` on the variant name
    /// `BlankFailure`); it is unrelated to the `phase` log literals emitted
    /// by [`DaemonEvent::DisplayPhase`] — don't conflate the two when
    /// grepping.
    BlankFailure {
        /// The display whose blank command failed.
        display: DisplayId,
        /// Name of the controller that failed (from the folded
        /// [`crate::types::CmdFailure`]).
        #[serde(default)]
        controller: String,
        /// Error detail, starting with an `E_*` code (from the folded
        /// [`crate::types::CmdFailure`]).
        #[serde(default)]
        detail: String,
    },
    /// A display's blank command succeeded after a prior [`Self::BlankFailure`]
    /// — the failed-blank condition has cleared.  Emitted at most once per
    /// failure (no repeat spam while healthy).
    BlankRecovered {
        /// The display whose blank command recovered.
        display: DisplayId,
    },
    /// A display's wake command succeeded after one or more prior
    /// [`DaemonEvent::WakeRetry`] broadcasts.  `display` is required;
    /// `attempts` is `#[serde(default)]` for forward compat.
    WakeRecovered {
        /// The display whose wake command recovered.
        display: DisplayId,
        /// How many retry attempts preceded the success.
        #[serde(default)]
        attempts: u64,
    },
    /// Ownership verdict changed or was confirmed by a write — the
    /// definitive answer to "who has the panel right now?".  Emitted by
    /// the direct-switch path on pull/push terminal outcomes and by the
    /// coordination poller on debounced ownership transitions.
    Ownership {
        /// The shared display whose ownership changed.
        display: DisplayId,
        /// Whether this machine currently owns the display.
        owned: bool,
        /// Last observed VCP `0x60` input-source code, when available
        /// (populated by poll; write path sets this from readback when captured).
        #[serde(default)]
        observed_input_code: Option<u8>,
        /// VCP `0x60` code that was written, when this event originates from a
        /// write path (pull/push). `None` for poll-observed events.
        #[serde(default)]
        written_code: Option<u8>,
        /// What triggered this ownership event.
        /// `"pull"` | `"push"` | `"poll"` | `"activity_follow"` | `"hotkey"` | `"cli"` | `"tray"` | `"web"`
        cause: String,
        /// When `cause` is a write path: did readback verify the panel moved?
        /// `None` when the event is from a read-only observation (poll).
        #[serde(default)]
        verified: Option<bool>,
        /// Set when the write path degraded (peer READ alias absent) —
        /// mirrors the existing `kvm_push_verification_degraded` log anchor.
        #[serde(default)]
        degraded: bool,
    },
    /// First frame emitted on a fresh event-stream connection, per-connection
    /// (never broadcast). Marks that the daemon-side broadcast receiver is
    /// registered, so events emitted after this line will be delivered.
    /// Consumers may ignore it.
    Subscribed,
    /// Web-scoped single-flight guard state changed — published by the
    /// `dormant-web` HTTP layer after every `exercise_in_flight` /
    /// `emergency_wake_lock` mutation (insert AND remove, including the
    /// detached completion monitor that outlives an HTTP timeout). Lets the
    /// webui replace a 1 Hz paired poll with event-driven UI, while still
    /// hitting `GET /api/operations` on initial load and on reconnect/refocus.
    /// `exercise_in_flight` is sorted ascending on the wire (grep-stable).
    OperationsChanged {
        /// Configured display ids with a web exercise currently awaiting
        /// engine completion.
        exercise_in_flight: Vec<String>,
        /// Whether a global web emergency wake is currently awaiting engine
        /// completion.
        emergency_wake_in_flight: bool,
    },
    /// Wire-tolerance catch-all: any event tag this build does not
    /// recognize deserializes to this variant instead of failing the whole
    /// stream.  The daemon never constructs this — see
    /// [`ControlMsg::PublishDaemonEvent`]'s debug assertion.  `#[doc(hidden)]`
    /// because it is not a real event kind, just the forward-compat escape
    /// hatch for older CLIs/WebUI builds talking to a newer daemon.
    #[doc(hidden)]
    #[serde(other)]
    Unknown,
}

/// A sensor as seen by a [`StateSnapshot`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SensorSnapshot {
    /// The sensor id (as a string for JSON readability).
    pub id: String,
    /// Current sensor state.
    pub state: SensorState,
    /// Seconds since the last event arrived from this sensor.
    pub last_seen_secs_ago: u64,
    /// Whether this sensor has delivered at least one event since daemon
    /// start (carried across config reloads). `false` = the state shown is
    /// the fail-safe seed, not information from the device.
    /// `#[serde(default)]` for legacy wire back-compat (older snapshots
    /// have no `reported` key; the honest default is `false` — unknown
    /// provenance).
    #[serde(default)]
    pub reported: bool,
}

/// A zone as seen by a [`StateSnapshot`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ZoneSnapshot {
    /// The zone id.
    pub id: String,
    /// Resolved presence (`None` if the zone is unknown to the engine).
    pub present: Option<bool>,
}

/// The role a controller plays in the ordered chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerRole {
    /// First controller in the chain — the preferred target.
    Primary,
    /// Any controller after the first — tried when the primary (and preceding
    /// fallbacks) fail.
    Fallback,
}

/// Per-controller health, recorded from the LAST blank/wake attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerHealth {
    /// Controller name literal (matches the config `type` key).
    pub name: String,
    /// Position in the chain.
    pub role: ControllerRole,
    /// Whether the last attempt succeeded.
    pub healthy: bool,
    /// Failure detail when `healthy` is false (`None` on success or before
    /// first attempt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The active ladder stage of a display in the `staged` phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageInfo {
    /// Zero-based index into the display's normalized ladder.
    pub idx: usize,
    /// The stage kind at that index.
    pub kind: StageKind,
}

/// A display as seen by a [`StateSnapshot`].
#[allow(
    clippy::struct_excessive_bools,
    reason = "the status wire mirrors independent display machine and ownership flags"
)]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplaySnapshot {
    /// Literal phase name (`"active"`, `"grace"`, `"blanking"`, `"blanked"`,
    /// `"waking"`).
    pub phase: String,
    /// Whether the user-activity inhibitor is engaged.
    pub inhibited: bool,
    /// Whether a manual/scheduled pause is active.
    pub paused: bool,
    /// The display machine's command-generation counter (carry-over across
    /// reloads).
    pub cmd_gen: u64,
    /// Whether this display is private to this daemon or coordinated between machines.
    #[serde(default)]
    pub scope: DisplayScope,
    /// Whether this daemon currently owns a shared display. Always `true` for private displays.
    #[serde(default = "default_owned")]
    pub owned: bool,
    /// Last observed shared-display input source. Absent for private or stale displays.
    #[serde(default)]
    pub observed_input_code: Option<u8>,
    /// Panel state observed with the shared-display input source. `None` is unknown or stale.
    #[serde(default)]
    pub panel_state: Option<PanelState>,
    /// Per-controller health from the last blank/wake attempt.  Empty until
    /// the first attempt or when deserializing legacy snapshots without this
    /// field (serde back-compat).
    #[serde(default)]
    pub controllers: Vec<ControllerHealth>,
    /// Current wake-retry attempt counter for this display (0 once healthy
    /// or before the first attempt).  `#[serde(default)]` for legacy wire
    /// back-compat.
    #[serde(default)]
    pub wake_attempts: u64,
    /// Whether the last blank attempt for this display exhausted its
    /// controller chain and has not yet recovered.  `#[serde(default)]` for
    /// legacy wire back-compat.
    #[serde(default)]
    pub last_blank_failed: bool,
    /// The active ladder stage when the display is in the `staged` phase.
    /// `None` for every other phase (and for legacy wire — the key is
    /// omitted when `None`, byte-identical to a pre-stage snapshot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<StageInfo>,
}

const fn default_owned() -> bool {
    true
}

/// Boot-time rollback metadata surfaced to operators through [`StateSnapshot`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RollbackStatus {
    /// Fingerprint of the operator config that failed.
    pub failed_fp: String,
    /// Fingerprint of the last-known-good config now running.
    pub lkg_fp: String,
    /// Human-readable rollback reason; matches the pending-reload rollback detail.
    pub detail: String,
    /// Suggested command to restart the daemon after fixing the config.
    /// Platform-specific best-effort suggestion (e.g.
    /// `"systemctl --user restart dormant"` on Linux,
    /// `"launchctl kickstart -k gui/501/dev.legionworks.dormant"` on macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_command: Option<String>,
}

/// A point-in-time view of engine state, returned by [`ControlMsg::Snapshot`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StateSnapshot {
    /// All sensors in the engine's inventory.
    pub sensors: Vec<SensorSnapshot>,
    /// All known zones.
    pub zones: Vec<ZoneSnapshot>,
    /// All displays keyed by id.
    pub displays: Vec<(String, DisplaySnapshot)>,
    /// `Some(detail)` when a config reload is pending (operator feedback).
    pub pending_reload: Option<String>,
    /// Boot-time rollback metadata, omitted when no rollback is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackStatus>,
    /// KVM-switch status payload (resolved keymap, switch-capable
    /// display set, activity-follow state). Additive — older
    /// clients omit the key. `#[serde(default)]` so legacy
    /// snapshots without the key deserialize cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kvm: Option<KvmStatus>,
    /// Redacted active wear-sampling lifecycle state, when the platform provides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wear_sampling_status: Option<crate::wear::WearSamplingStatus>,
}

/// KVM-switch snapshot payload — the tray refetches this on every
/// `ConfigReloaded` event (spec §3, IPC contract).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct KvmStatus {
    /// Resolved keymap (`keymap.claim_hotkey`).
    #[serde(default)]
    pub keymap: crate::config::KeymapConfig,
    /// Post-probe switch-capable display set (shared scope AND
    /// input-source write capable AND configured local read code).
    /// Claim identity no longer participates.
    #[serde(default)]
    pub switch_capable_displays: Vec<crate::types::DisplayId>,
    /// Whether the activity-follow task is running.
    #[serde(default)]
    pub activity_following: bool,
    /// Post-probe push-capable display ids — shared scope, write capable,
    /// with `shared_peer_input_write_code` configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_capable_displays: Vec<crate::types::DisplayId>,
}

// ── Per-runtime configuration shapes ─────────────────────────────────────────

/// Per-display runtime configuration for the engine.
#[derive(Debug, Clone)]
pub struct DisplayRuntimeCfg {
    /// The display this config applies to.
    pub display: DisplayId,
    /// The primary blank mode — the first controller stage's mode.
    pub blank_mode: BlankMode,
    /// The display's escalation ladder.
    pub ladder: Vec<LadderStage>,
    /// Timing parameters for the display's state machine.
    pub timings: SmTimings,
}

impl DisplayRuntimeCfg {
    /// Timing defaults for a manual-only (rule-less) display.  `grace` and
    /// `min_*` times are moot without a zone; wake-retry is preserved — a
    /// failed manual wake must self-heal (the wake-wedge invariant).
    #[must_use]
    pub fn manual_defaults(startup_holdoff: std::time::Duration) -> SmTimings {
        use crate::config::defaults;
        SmTimings {
            grace_period: defaults::GRACE_PERIOD,
            min_blank_time: defaults::MIN_BLANK_TIME,
            min_wake_time: defaults::MIN_WAKE_TIME,
            startup_holdoff,
            wake_retry_interval: defaults::WAKE_RETRY_INTERVAL,
        }
    }
}

/// Per-rule runtime configuration for the engine.
#[derive(Debug, Clone)]
pub struct RuleRuntimeCfg {
    /// The rule id.
    pub rule: RuleId,
    /// The zone whose resolved presence drives this rule.
    pub zone: ZoneId,
    /// The displays to step when the zone flips.
    pub displays: Vec<DisplayId>,
    /// How long to hold a display awake after a render-surface input wake
    /// when every driving zone is vacant (`0s` disables).
    pub input_wake_hold: Duration,
}

/// Per-sensor runtime configuration for the engine.
#[derive(Debug, Clone)]
pub struct SensorRuntimeCfg {
    /// The sensor id.
    pub sensor: SensorId,
    /// How this sensor's events are interpreted.
    pub kind: SensorKind,
    /// Motion-sensor hold-time override (`Some(h)` → stretch pulses by `h`;
    /// `None` → no hold; ignored unless `kind == Motion`).
    pub hold_time: Option<Duration>,
    /// After this much wall-clock silence without an event, the sensor is
    /// marked `Unavailable` by the sweeper.
    pub stale_timeout: Duration,
}

/// The complete runtime configuration handed to [`RulesEngine::new`].
#[derive(Debug, Clone)]
pub struct RulesEngineConfig {
    /// All rules (zone → displays).
    pub rules: Vec<RuleRuntimeCfg>,
    /// All displays (must include every display referenced by any rule).
    pub displays: Vec<DisplayRuntimeCfg>,
    /// All sensors (must include every sensor referenced by any zone spec
    /// passed to [`RulesEngine::new`]).
    pub sensors: Vec<SensorRuntimeCfg>,
    /// Post-wake settle window for the doctor exercise's bounded retry
    /// read (`daemon.doctor_wake_settle`, default `3s`). When the first
    /// post-wake read is absent or still non-`On`, the exercise sleeps
    /// this long and performs exactly one more bounded read before
    /// classifying the wake step.
    pub doctor_wake_settle: Duration,
}

// ── Internal types ────────────────────────────────────────────────────────────

/// A timer-wheel entry — discriminated by kind so the dispatcher knows what
/// to do when it fires.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TimerEntry {
    /// Drive [`DisplayStateMachine::step`] with [`Input::Tick`] at deadline.
    DisplayTick(DisplayId),
    /// Drive [`DisplayStateMachine::step`] with [`Input::StageTick`] at
    /// deadline, carrying the generation counter for stale-detection.
    DisplayStageTick(DisplayId, u64),
    /// Hold-time expiry: synthesize the held Absent event for this sensor.
    HoldExpiry(SensorId),
    /// Input-wake hold expiry: remove the hold and re-enter the normal
    /// grace path for this display (issue #125).
    InputWakeHoldExpiry(DisplayId),
}

/// Reply from a spawned dispatch task back to the engine.
#[derive(Debug)]
enum InternalResult {
    /// Result of a blank command issued earlier.
    Blank {
        /// Display the command was issued for.
        display: DisplayId,
        /// Generation counter matching the `IssueBlank` effect.
        r#gen: u64,
        /// Outcome.
        result: Result<(), CmdFailure>,
    },
    /// Result of a wake command issued earlier.
    Wake {
        /// Display the command was issued for.
        display: DisplayId,
        /// Generation counter matching the `IssueWake` effect.
        r#gen: u64,
        /// Outcome.
        result: Result<(), CmdFailure>,
    },
    /// Result of a render command issued earlier.
    Render {
        /// Display the command was issued for.
        display: DisplayId,
        /// Generation counter matching the `ShowRender` effect.
        r#gen: u64,
        /// Outcome.
        result: Result<(), CmdFailure>,
    },
    /// Resume the listed rules — sent by the off-run-loop exercise
    /// sequence so the pause window is released ENGINE-SIDE, independent
    /// of whether the IPC caller is still listening.  Routes through
    /// the internal results channel so the run loop applies the resume
    /// with `&mut self` — guaranteed as long as the engine is alive
    /// (the IPC timeout / dropped-reply paths can no longer strand a
    /// paused rule).  See `RulesEngine::handle_exercise`.
    ExerciseResume {
        /// Rule ids to resume (any that were paused for the exercise
        /// window).  Empty Vec is a no-op.
        rules: Vec<RuleId>,
        /// Operation identity used to fence completion after a reload.
        operation_id: u64,
        /// Generation that accepted the operation.
        generation: GenerationId,
    },
}

/// Releases an exercise pause even when its detached task is cancelled.
struct ExercisePauseGuard {
    results_tx: mpsc::UnboundedSender<InternalResult>,
    rules: Option<Vec<RuleId>>,
    operation_id: u64,
    generation: GenerationId,
}

impl Drop for ExercisePauseGuard {
    fn drop(&mut self) {
        if let Some(rules) = self.rules.take() {
            let _ = self.results_tx.send(InternalResult::ExerciseResume {
                rules,
                operation_id: self.operation_id,
                generation: self.generation,
            });
        }
    }
}

/// One rule's per-kind inhibitor bookkeeping.
///
/// `kinds` records the last-known engaged/released bool per
/// [`InhibitorKind`] this rule has ever seen (kinds never reported for this
/// rule are simply absent — treated as not-engaged by the OR-derivation).
/// `effective` is the OR of every value in `kinds`, cached so
/// [`RulesEngine::apply_inhibitor`] can detect a no-op update (a kind flip
/// that doesn't change the OR result) without recomputing against the
/// previous state machine input.
#[derive(Debug, Default, Clone)]
struct InhibitorSet {
    /// Last-known engaged/released bool per inhibitor kind.
    kinds: HashMap<InhibitorKind, bool>,
    /// OR of every value in `kinds` — the bit actually fed to
    /// [`Input::InhibitorChanged`].
    effective: bool,
}

/// Motion-sensor hold state — one entry per sensor with `kind == Motion` and
/// `hold_time = Some(_)`.
#[derive(Debug, Default, Clone)]
struct HoldState {
    /// Deadline of the currently-armed hold timer, if any.
    armed_until: Option<Tick>,
    /// Absent event received while the hold was armed (swallowed; replayed on
    /// expiry).
    pending_absent: Option<PresenceEvent>,
}

// ── RulesEngine ───────────────────────────────────────────────────────────────

/// The async rules engine.  Construct with [`RulesEngine::new`], drive with
/// [`RulesEngine::run`].
///
/// All fields are private — the public API is the constructor, the run
/// future, and the two pre-run mutators [`RulesEngine::set_pending_reload`]
/// and [`RulesEngine::apply_restore_effects`].
pub struct RulesEngine {
    /// Frozen per-runtime config.
    cfg: RulesEngineConfig,
    /// Zone fusion engine — owned.
    zone_engine: ZoneEngine,
    /// Per-display state machines, keyed by display id.
    machines: HashMap<DisplayId, DisplayStateMachine>,
    /// Per-display command executors (the I/O side).
    executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
    /// Per-display render sinks (the surface I/O side).  Empty when no render
    /// backend is injected; render stages then fall through.
    render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>>,
    /// Ownership gate — consulted every time the engine interacts with a
    /// display.  Feeds [`Input::OwnershipChanged`] to the state machine when
    /// the gate's verdict differs from the last-fed value.
    ownership: Arc<dyn OwnershipGate>,
    coordination: Option<CoordinationHandle>,
    /// Last ownership value fed per display — change-detection to avoid
    /// redundant [`Input::OwnershipChanged`] edges every tick.
    last_owned: HashMap<DisplayId, bool>,
    /// Rule → its displays.
    rule_displays: HashMap<RuleId, Vec<DisplayId>>,
    /// Zone → rules bound to it.
    zone_rules: HashMap<ZoneId, Vec<RuleId>>,
    /// Sensors that are currently paused at the rule level (skip blanking).
    paused_rules: HashSet<RuleId>,
    /// Pause source scope retained so auto-resume events identify their rule.
    paused_scopes: HashMap<DisplayId, Option<RuleId>>,
    /// Per-rule inhibitor bookkeeping (kind → engaged, plus the OR-derived
    /// effective bit). A rule absent from this map has never received a
    /// [`ControlMsg::SetInhibited`] — equivalent to an all-`false`
    /// [`InhibitorSet`].
    inhibitor_state: HashMap<RuleId, InhibitorSet>,
    /// Per-sensor hold state (only populated for `Motion + Some(hold_time)`).
    holds: HashMap<SensorId, HoldState>,
    /// Per-display wake-retry counter (increments on each failure, resets on
    /// success).
    wake_attempts: HashMap<DisplayId, u64>,
    /// Sensors that have delivered at least one [`PresenceEvent`] since
    /// daemon start (any state — this records "has reported", not "is
    /// present").  Diagnostic-only: deliberately SEPARATE from
    /// `sensor_last_seen_virtual` (seeding that map would perturb
    /// stale-sweep semantics).  Surfaced via [`SensorSnapshot::reported`].
    reported: HashSet<SensorId>,
    /// Displays whose last blank attempt exhausted its controller chain and
    /// has not yet recovered.  Sibling of `wake_attempts` — same
    /// insert-on-failure / remove-on-success bookkeeping, but as a set since
    /// blank failure has no attempt counter (spec §3.1).
    last_blank_failed: HashSet<DisplayId>,
    /// Virtual last-seen per sensor — drives the stale-sensor sweep using
    /// the tokio clock so paused tests can advance minutes in milliseconds.
    sensor_last_seen_virtual: HashMap<SensorId, tokio::time::Instant>,
    /// Sensors whose source has asserted `online = true` via a recent
    /// availability (LWT) edge, mapped to the virtual (tokio) instant
    /// when that assertion was recorded. The stale sweep consults this
    /// map to decide whether a sensor's presence-topic silence may be
    /// treated as "state unchanged" (in-map and lease still valid) or
    /// "device gone" (not in map, OR lease expired → fire
    /// `Unavailable`). The lease is bounded by the sensor's configured
    /// `stale_timeout` — a `retained online` that goes truly silent
    /// past `stale_timeout` must still expire, otherwise a dead
    /// connection whose last broker-side state was `online` could
    /// preserve stale presence forever (issue #205).
    ///
    /// Clearing sites: [`RulesEngine::handle_presence_event`]
    /// (`state == Unavailable` path),
    /// [`RulesEngine::handle_sensor_availability`] (offline LWT), and
    /// [`RulesEngine::sweep_stale_sensors`] (lease expired).
    /// Consumed by [`RulesEngine::sweep_stale_sensors`].
    /// See also [`RulesEngine::input_wake_holds`] for the analogous
    /// per-display hold expiry path.
    availability_online: HashMap<SensorId, tokio::time::Instant>,
    /// Timer wheel — min-heap on `(Tick, entry)`.
    timers: BinaryHeap<Reverse<(Tick, TimerEntry)>>,
    /// Internal results mpsc — spawned dispatch tasks write here.
    results_rx: mpsc::UnboundedReceiver<InternalResult>,
    /// Internal results mpsc — cloned into each spawned dispatch task.
    results_tx: mpsc::UnboundedSender<InternalResult>,
    /// Broadcast bus for [`DaemonEvent`]s.
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Optional daemon-level diagnostic sink for lifecycle transitions.
    observations: Option<(GenerationId, ObservationHub)>,
    /// Shared daemon-lifetime operation lease registry.
    operation_registry: OperationRegistry,
    /// Pending reload detail (operator feedback in snapshots).
    pending_reload: Option<String>,
    /// Boot-time rollback metadata (operator feedback in snapshots).
    rollback: Option<RollbackStatus>,
    /// Pre-run effects queued by [`RulesEngine::apply_restore_effects`].
    /// Drained into timer / dispatch structures at the start of
    /// [`RulesEngine::run`].
    pending_restore: Vec<(DisplayId, Vec<Effect>)>,
    /// KVM-switch snapshot payload (keymap + claim-capable
    /// displays + activity-claim policy). Updated by the
    /// orchestrator on every successful generation install; the
    /// rules engine reads it from
    /// [`RulesEngine::send_snapshot`]. `None` until the first
    /// orchestrator-side install completes.
    kvm: Option<KvmStatus>,
    /// Per-display F10 suppression deadlines (set by
    /// [`ControlMsg::SetClaimSuppression`]). When the rules engine
    /// receives a `ControlMsg::OwnershipPoll` for a display whose
    /// suppression deadline is in the future, the ownership-loss
    /// reaction is suppressed (the engine does not feed
    /// `Input::OwnershipChanged(false)` to the state machine). The
    /// claim runtime owns this side-table; the orchestrator wires
    /// it through the standard control channel.
    #[allow(clippy::type_complexity)]
    claim_suppression: HashMap<DisplayId, Instant>,
    /// Per-display input-wake hold deadlines (issue #125).  When a render-
    /// surface `InputWake` arrives while every zone that drives this display
    /// is vacant and the rule's `input_wake_hold` is > 0, a deadline
    /// (`now + hold`) is stored here and a [`TimerEntry::InputWakeHoldExpiry`]
    /// is scheduled.  While the deadline is in the future, the state machine's
    /// `input_wake_hold_active` flag prevents the deferred Grace chain in
    /// [`DisplayStateMachine::enter_active`].
    ///
    /// Clearing sites: [`RulesEngine::fan_zone_change_to_displays`]
    /// (when presence returns, `present == true`) and
    /// [`RulesEngine::fire_input_wake_hold_expiry`] (hold expiry).
    /// Both clear the hold map entry AND the state machine flag.
    /// NOT carried across reload — see `apply_restore` in `dormantd`.
    /// See also [`RulesEngine::availability_online`] for the analogous
    /// cleared-on-presence pattern.
    input_wake_holds: HashMap<DisplayId, Instant>,
}

impl RulesEngine {
    /// Construct an engine from its runtime config, zone engine, command
    /// executors, render sinks, and ownership gate.
    ///
    /// # Errors
    ///
    /// [`DormantError::ConfigInvalid`] if any rule references a display that
    /// has no [`DisplayRuntimeCfg`] and no [`CommandSink`] executor.
    pub fn new(
        cfg: RulesEngineConfig,
        zone_engine: ZoneEngine,
        executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
        render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>>,
        ownership: Arc<dyn OwnershipGate>,
    ) -> Result<Self, DormantError> {
        let now = Tick::now();

        // Build machines for every declared display.
        let mut machines: HashMap<DisplayId, DisplayStateMachine> = HashMap::new();
        for dcfg in &cfg.displays {
            machines.insert(
                dcfg.display.clone(),
                DisplayStateMachine::new(dcfg.timings.clone(), dcfg.ladder.clone(), now),
            );
        }

        // Seed ownership state into each machine so it starts with the
        // correct gate verdict.  Machines default to `owned: true`; this
        // initial feed brings them in sync with the gate before the first
        // event.
        let mut last_owned: HashMap<DisplayId, bool> = HashMap::new();
        for (display_id, machine) in &mut machines {
            let owns = ownership.owns(display_id);
            let _ = machine.step(Input::OwnershipChanged(owns), now);
            last_owned.insert(display_id.clone(), owns);
        }

        // Validate cross-references and build index structures.
        let mut rule_displays: HashMap<RuleId, Vec<DisplayId>> = HashMap::new();
        let mut zone_rules: HashMap<ZoneId, Vec<RuleId>> = HashMap::new();
        for rule in &cfg.rules {
            for display_id in &rule.displays {
                if !machines.contains_key(display_id) {
                    return Err(DormantError::ConfigInvalid {
                        detail: format!(
                            "rule '{}' references display '{}' with no DisplayRuntimeCfg",
                            rule.rule, display_id
                        ),
                    });
                }
                if !executors.contains_key(display_id) {
                    return Err(DormantError::ConfigInvalid {
                        detail: format!(
                            "rule '{}' references display '{}' with no CommandSink",
                            rule.rule, display_id
                        ),
                    });
                }
            }
            rule_displays.insert(rule.rule.clone(), rule.displays.clone());
            zone_rules
                .entry(rule.zone.clone())
                .or_default()
                .push(rule.rule.clone());
        }

        // Per-sensor hold state — only populated when the sensor is Motion
        // with a hold_time override.  All other sensors pass through.
        let mut holds: HashMap<SensorId, HoldState> = HashMap::new();
        for scfg in &cfg.sensors {
            if scfg.kind == SensorKind::Motion && scfg.hold_time.is_some() {
                holds.insert(scfg.sensor.clone(), HoldState::default());
            }
        }

        let (results_tx, results_rx) = mpsc::unbounded_channel();
        let (event_tx, _) = broadcast::channel(256);

        Ok(Self {
            cfg,
            zone_engine,
            machines,
            executors,
            render_sinks,
            ownership,
            coordination: None,
            last_owned,
            rule_displays,
            zone_rules,
            paused_rules: HashSet::new(),
            paused_scopes: HashMap::new(),
            inhibitor_state: HashMap::new(),
            holds,
            wake_attempts: HashMap::new(),
            reported: HashSet::new(),
            last_blank_failed: HashSet::new(),
            sensor_last_seen_virtual: HashMap::new(),
            availability_online: HashMap::new(),
            timers: BinaryHeap::new(),
            results_rx,
            results_tx,
            event_tx,
            observations: None,
            operation_registry: OperationRegistry::default(),
            pending_reload: None,
            rollback: None,
            kvm: None,
            claim_suppression: HashMap::new(),
            input_wake_holds: HashMap::new(),
            pending_restore: Vec::new(),
        })
    }

    /// Attach the daemon-lifetime shared-display observation cache.
    #[must_use]
    pub fn with_coordination_handle(mut self, coordination: CoordinationHandle) -> Self {
        self.coordination = Some(coordination);
        self
    }

    /// Attach the daemon observation hub for this engine generation.
    #[must_use]
    pub fn with_observation_hub(mut self, generation: GenerationId, hub: ObservationHub) -> Self {
        self.observations = Some((generation, hub));
        self
    }

    /// Attach the daemon-lifetime operation registry shared across reloads.
    #[must_use]
    pub fn with_operation_registry(mut self, registry: OperationRegistry) -> Self {
        self.operation_registry = registry;
        self
    }

    /// Set or clear the pending-reload indicator (operator feedback in
    /// [`StateSnapshot`]s).
    pub fn set_pending_reload(&mut self, detail: Option<String>) {
        self.pending_reload = detail;
    }

    /// Set or clear the rollback indicator exposed by [`StateSnapshot`].
    pub fn set_rollback(&mut self, status: Option<RollbackStatus>) {
        self.rollback = status;
    }

    /// Queue effects from a freshly-restored display machine.  Stored
    /// pre-run and drained into the timer wheel / dispatch structures when
    /// [`RulesEngine::run`] starts.
    pub fn apply_restore_effects(&mut self, display: &DisplayId, effects: Vec<Effect>) {
        self.pending_restore.push((display.clone(), effects));
    }

    /// Replace a display's state machine with a restored one, re-seed its
    /// ownership from the gate (keeps `last_owned` in sync — the restored
    /// machine defaults `owned: true`), and queue its initial scheduling
    /// effects.
    ///
    /// Used by the daemon's reload path to preserve a manual-only display's
    /// phase across reload (M1 deferred this seam).  Call only for a display
    /// present in `self.machines`; a no-op otherwise.
    pub fn install_restored_machine(
        &mut self,
        display: &DisplayId,
        machine: DisplayStateMachine,
        effects: Vec<Effect>,
        now: Tick,
    ) {
        let mut phase_changes = Vec::new();
        if let Some(slot) = self.machines.get_mut(display) {
            let old_phase = slot.phase().clone();
            *slot = machine;
            let restored_phase = slot.phase().clone();
            if old_phase != restored_phase {
                phase_changes.push((old_phase, restored_phase));
            }
            let owns = self.ownership.owns(display);
            let (refeed, transition) =
                slot.step_with_transition(Input::OwnershipChanged(owns), now);
            if let Some(transition) = transition {
                phase_changes.push(transition);
            }
            self.last_owned.insert(display.clone(), owns);
            // Queue restore-phase-entry effects first, then the
            // ownership-edge effects — both drain via process_effect at
            // run() start.  The re-feed is NOT a no-op for every
            // (phase, owns) pair; an owns:false restore into Blanked/
            // Staged/RenderPending emits TeardownRender / LogTransition
            // that must reach dispatch.
            let mut queued = effects;
            queued.extend(refeed);
            self.pending_restore.push((display.clone(), queued));
        }
        for (old_phase, new_phase) in phase_changes {
            self.emit_phase_transition(display, old_phase, new_phase);
        }
    }

    /// Seed pre-run wake/blank failure bookkeeping for a display — used by
    /// the daemon's reload/restore path (T3) to carry a display's failure
    /// state across a reload alongside [`RulesEngine::install_restored_machine`],
    /// so an operator watching `dormantctl watch`/the snapshot doesn't see a
    /// spurious "recovered" event (or lose an in-flight failure indicator)
    /// purely because the engine was rebuilt.  A no-op for a fresh
    /// (never-failed) display: `wake_attempts == 0` and
    /// `last_blank_failed == false` insert nothing.
    pub fn seed_failure_state(
        &mut self,
        display: &DisplayId,
        wake_attempts: u64,
        last_blank_failed: bool,
    ) {
        if wake_attempts > 0 {
            self.wake_attempts.insert(display.clone(), wake_attempts);
        }
        if last_blank_failed {
            self.last_blank_failed.insert(display.clone());
        }
    }

    /// Seed the diagnostic "has reported since daemon start" bit for a
    /// sensor — used by the daemon's reload/restore path to carry the
    /// `reported` flag across a reload (sibling seam to
    /// [`RulesEngine::seed_failure_state`]).  The caller is responsible for
    /// only seeding sensors whose binding is unchanged across the reload
    /// (provenance discipline lives in the daemon's reload helper, not
    /// here); this method just records the bit.
    pub fn seed_sensor_reported(&mut self, sensor: &SensorId) {
        self.reported.insert(sensor.clone());
    }

    /// Drive the engine until `cancel` is triggered or both inbound channels
    /// close.  Consumes `self`.
    pub async fn run(
        mut self,
        mut events: mpsc::Receiver<PresenceEvent>,
        mut ctl: mpsc::Receiver<ControlMsg>,
        cancel: CancellationToken,
    ) {
        // ── Drain pre-run restore effects into live structures. ────────────
        let drained: Vec<(DisplayId, Vec<Effect>)> = self.pending_restore.drain(..).collect();
        for (display, effects) in drained {
            for effect in effects {
                self.process_effect(&display, effect);
            }
        }

        // ── Initial sweep setup. ────────────────────────────────────────────
        let sweep_period = self.compute_sweep_period();
        let mut next_sweep = Tick(Tick::now().0 + sweep_period);

        // ── Main loop. ──────────────────────────────────────────────────────
        loop {
            let now_tick = Tick::now();

            // Earliest timer deadline (or far-future pending if heap empty).
            let timer_fut: Pin<Box<dyn Future<Output = ()> + Send>> = match self.timers.peek() {
                Some(Reverse((tick, _))) => {
                    Box::pin(tokio::time::sleep_until(to_tokio_instant(tick.0)))
                }
                None => Box::pin(std::future::pending::<()>()),
            };

            let sweep_deadline = next_sweep.0;
            let sweep_fut: Pin<Box<dyn Future<Output = ()> + Send>> =
                Box::pin(tokio::time::sleep_until(to_tokio_instant(sweep_deadline)));

            tokio::select! {
                biased;

                () = cancel.cancelled() => break,
                ev = events.recv() => {
                    match ev {
                        Some(e) => self.handle_presence_event(e),
                        None => break,
                    }
                }
                c = ctl.recv() => {
                    match c {
                        Some(ControlMsg::GenerationBarrier(ack)) => {
                            self.drain_generation_inputs(&mut events, &mut ctl);
                            let _ = ack.send(());
                        }
                        Some(c) => self.handle_control(c),
                        None => break,
                    }
                }
                res = self.results_rx.recv() => {
                    if let Some(r) = res {
                        self.handle_internal_result(r, now_tick);
                    }
                    // None = no senders alive — fall through; the loop will
                    // re-enter select!.  Other arms (events, ctl, timer) keep
                    // us live.
                }
                () = timer_fut => self.fire_due_timers(now_tick),
                () = sweep_fut => {
                    self.sweep_stale_sensors();
                    next_sweep = Tick(now_tick.0 + sweep_period);
                }
            }
        }
    }

    // ── Internal: presence events ───────────────────────────────────────────

    /// Run a presence event through the hold-filter → zone engine → display
    /// pipeline.
    fn handle_presence_event(&mut self, ev: PresenceEvent) {
        // Diagnostic-only: record that this sensor has reported at least
        // once since daemon start — ANY state (Present/Absent/Unavailable)
        // counts as "has reported".  Deliberately recorded from the RAW
        // event, BEFORE the hold filter runs, so "has reported since
        // start" is unconditional on every `PresenceEvent` received by
        // construction, not merely as an implicit consequence of
        // `HoldState` always starting disarmed (T3 review S1 — a future
        // change to `apply_hold_filter`, e.g. a hold that could start
        // pre-armed, must not be able to silently break this).
        // Deliberately separate from `sensor_last_seen_virtual` below (see
        // the field doc on `RulesEngine::reported`).
        self.reported.insert(ev.sensor_id.clone());

        // Clear the source-asserted `online` availability claim whenever a
        // sensor reports Unavailable — the same code path covers LWT
        // `offline` payloads (already synthesised by
        // `handle_sensor_availability` above) and broker/source failures
        // (the source emits Unavailable for every owned sensor). Without
        // this, a dead connection whose last broker-side state was `online`
        // could preserve stale presence forever by gating the sweep.
        if ev.state == SensorState::Unavailable {
            self.availability_online.remove(&ev.sensor_id);
        }

        // Hold-filter: swallow / arm / pass through based on the sensor's
        // kind and hold_time.
        let effective = self.apply_hold_filter(ev);

        let Some(effective) = effective else {
            // Swallowed by hold filter.
            return;
        };

        // Track virtual last-seen for the stale sweep (tokio clock so paused
        // tests can drive minutes in milliseconds).
        self.sensor_last_seen_virtual
            .insert(effective.sensor_id.clone(), tokio::time::Instant::now());

        // SensorChanged broadcast — only if the state actually changed from
        // what the zone engine has recorded.
        let prior_state = self
            .zone_engine
            .sensor_states()
            .get(&effective.sensor_id)
            .map(|(s, _)| *s);
        if prior_state != Some(effective.state) {
            let _ = self.event_tx.send(DaemonEvent::SensorChanged {
                sensor: effective.sensor_id.clone(),
                state: effective.state,
            });
        }

        // Zone fusion.
        let changes = self.zone_engine.apply(&effective);
        for change in changes {
            let _ = self.event_tx.send(DaemonEvent::ZoneChanged {
                zone: change.zone.clone(),
                present: change.present,
                cause: change.cause.clone(),
            });
            self.fan_zone_change_to_displays(&change.zone, change.present);
        }
    }

    /// Apply motion-sensor hold-time semantics.  Returns `None` if the event
    /// was swallowed by an armed hold.
    fn apply_hold_filter(&mut self, ev: PresenceEvent) -> Option<PresenceEvent> {
        // Pass-through if this sensor has no hold state configured.
        if !self.holds.contains_key(&ev.sensor_id) {
            return Some(ev);
        }
        let hold = self.holds.get_mut(&ev.sensor_id).expect("checked");

        match ev.state {
            SensorState::Present => {
                let h = self
                    .cfg
                    .sensors
                    .iter()
                    .find(|s| s.sensor == ev.sensor_id)
                    .and_then(|s| s.hold_time)?;
                let deadline = Tick(Tick::now().0 + h);
                hold.armed_until = Some(deadline);
                hold.pending_absent = None;
                self.timers.push(Reverse((
                    deadline,
                    TimerEntry::HoldExpiry(ev.sensor_id.clone()),
                )));
                Some(ev)
            }
            SensorState::Absent => {
                if hold.armed_until.is_some() {
                    hold.pending_absent = Some(ev);
                    None
                } else {
                    Some(ev)
                }
            }
            SensorState::Unavailable => {
                hold.armed_until = None;
                hold.pending_absent = None;
                Some(ev)
            }
        }
    }

    /// Drive every display machine bound to a rule on this zone through one
    /// `step(Input::ZonePresent(change.present))`.
    ///
    /// Paused rules are NOT skipped here — the state machine owns the
    /// pause semantics on its overlays (freeze blank path, leave wake
    /// unaffected, track the zone level so an un-paused machine is never
    /// surprised by a missed edge). `paused_rules` is kept as bookkeeping
    /// for `Resume` routing and snapshot reporting only.
    fn fan_zone_change_to_displays(&mut self, zone: &ZoneId, present: bool) {
        // Snapshot the rule ids so the immutable borrow on `self.zone_rules`
        // ends before we step machines mutably.
        let rule_ids: Vec<RuleId> = match self.zone_rules.get(zone) {
            Some(rs) => rs.clone(),
            None => return,
        };
        let now = Tick::now();
        for rule_id in rule_ids {
            let displays: Vec<DisplayId> = match self.rule_displays.get(&rule_id) {
                Some(ds) => ds.clone(),
                None => continue,
            };
            for display_id in displays {
                // Issue #125: when presence returns, clear any active
                // input-wake hold — the room is occupied, so there is
                // nothing to hold back.
                if present {
                    self.input_wake_holds.remove(&display_id);
                    if let Some(machine) = self.machines.get_mut(&display_id) {
                        machine.set_input_wake_hold_active(false);
                    }
                }
                // Feed ownership so the gate verdict is current before
                // processing the presence edge.
                self.feed_ownership(&display_id, now);
                self.step_machine(&display_id, Input::ZonePresent(present), now);
            }
        }
    }

    // ── Internal: control messages ─────────────────────────────────────────

    fn handle_control(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::Pause { rule, until } => self.handle_pause(rule.as_ref(), until),
            ControlMsg::Resume { rule } => self.handle_resume(rule.as_ref()),
            ControlMsg::ForceBlank(d) => self.step_one(&d, Input::ForceBlank),
            ControlMsg::ForceWake(d) => self.step_one(&d, Input::ForceWake),
            ControlMsg::SoftBlank(d) => self.step_one(&d, Input::SoftBlank),
            ControlMsg::InputWake(d) => {
                // Issue #125: if every zone that drives this display is
                // vacant and the effective `input_wake_hold` for the
                // applicable rule is > 0, arm the hold so the state
                // machine does not immediately chain back into Grace
                // after the wake.
                #[allow(clippy::collapsible_if)]
                if let Some(hold) = self.effective_input_wake_hold(&d) {
                    if hold > Duration::ZERO {
                        // Tick::now(), not std Instant::now(): the deadline is
                        // compared against the virtual-aware timer clock, and
                        // mixing clocks silently breaks under paused-time tests
                        // (the stale-timer guard compares Tick-derived instants).
                        let now = Tick::now().0;
                        let deadline = now + hold;
                        self.input_wake_holds.insert(d.clone(), deadline);
                        self.timers.push(Reverse((
                            Tick(deadline),
                            TimerEntry::InputWakeHoldExpiry(d.clone()),
                        )));
                        // Set the hold flag on the state machine so
                        // `enter_active` skips the deferred Grace chain.
                        if let Some(machine) = self.machines.get_mut(&d) {
                            machine.set_input_wake_hold_active(true);
                        }
                    }
                }
                self.step_one(&d, Input::InputWake);
            }
            ControlMsg::OwnershipPoll { display } => self.feed_ownership(&display, Tick::now()),
            ControlMsg::PublishDaemonEvent(ev) => {
                debug_assert!(
                    !matches!(ev, DaemonEvent::Unknown),
                    "daemon must never construct Unknown"
                );
                let _ = self.event_tx.send(ev);
            }
            ControlMsg::Snapshot(tx) => self.send_snapshot(tx),
            ControlMsg::SubscribeEvents(tx) => {
                let _ = tx.send(self.event_tx.subscribe());
            }
            ControlMsg::SetInhibited {
                rule,
                kind,
                inhibited,
            } => {
                self.handle_set_inhibited(rule.as_ref(), kind, inhibited);
            }
            ControlMsg::SetPendingReload(detail) => self.set_pending_reload(detail),
            ControlMsg::SetRollback(status) => self.set_rollback(status),
            ControlMsg::SetClaimSuppression { display, until } => match until {
                Some(deadline) => {
                    self.claim_suppression.insert(display, deadline);
                }
                None => {
                    self.claim_suppression.remove(&display);
                }
            },
            ControlMsg::SetKvmStatus(status) => {
                self.kvm = Some(status);
            }
            ControlMsg::EmergencyWake { reply } => self.handle_emergency_wake(reply, None),
            ControlMsg::EmergencyWakeAccepted { operation, reply } => {
                self.handle_emergency_wake(reply, Some(operation));
            }
            ControlMsg::Exercise { display, reply } => self.handle_exercise(display, reply, None),
            ControlMsg::ExerciseAccepted { operation, reply } => {
                let OperationKind::Exercise(ref display) = operation.kind else {
                    let _ = reply.send(ExerciseReport {
                        display: DisplayId("unknown".into()),
                        pre_phase: "unknown".into(),
                        operation_id: None,
                        generation: None,
                        paused_rules: Vec::new(),
                        steps: Vec::new(),
                    });
                    return;
                };
                self.handle_exercise(display.clone(), reply, Some(operation));
            }
            ControlMsg::GenerationBarrier(ack) => {
                let _ = ack.send(());
            }
            ControlMsg::SensorAvailability(ev) => self.handle_sensor_availability(ev),
        }
    }

    /// Record a source-asserted availability (LWT) edge.
    ///
    /// `online = true` records the sensor in [`Self::availability_online`]
    /// with the current virtual instant, so the stale sweep leaves it alone
    /// on topic silence until that lease expires (bounded by the sensor's
    /// `stale_timeout` — issue #205). Deliberately does NOT touch
    /// `sensor_last_seen_virtual` — the trap from issue #136: a heartbeat-style
    /// "online" frame would otherwise mask a real broker disconnect from the
    /// next sweep's view of elapsed time.
    ///
    /// `online = false` (LWT) removes the sensor from the map AND synthesises
    /// an `Unavailable` [`PresenceEvent`] so the fail-safe presence path
    /// takes over immediately, matching today's behavior for an `offline`
    /// payload.
    fn handle_sensor_availability(&mut self, ev: crate::types::SensorAvailabilityEvent) {
        if ev.online {
            self.availability_online
                .insert(ev.sensor, tokio::time::Instant::now());
        } else {
            self.availability_online.remove(&ev.sensor);
            self.handle_presence_event(PresenceEvent::new(
                ev.sensor,
                SensorState::Unavailable,
                ev.at,
            ));
        }
    }

    /// Consume inputs already buffered when a generation barrier is received.
    fn drain_generation_inputs(
        &mut self,
        events: &mut mpsc::Receiver<PresenceEvent>,
        ctl: &mut mpsc::Receiver<ControlMsg>,
    ) {
        while let Ok(event) = events.try_recv() {
            self.handle_presence_event(event);
        }
        while let Ok(control) = ctl.try_recv() {
            self.handle_control(control);
        }
    }

    /// Route an inhibitor state change to a rule (or every rule when `rule`
    /// is `None`), mirroring [`Self::handle_pause`]'s per-RULE bookkeeping
    /// idiom for the `None` case (NOT the bare `cfg.displays` fan-out —
    /// each rule's inhibitor state is tracked, and fanned out to displays,
    /// independently).
    fn handle_set_inhibited(
        &mut self,
        rule: Option<&RuleId>,
        kind: InhibitorKind,
        inhibited: bool,
    ) {
        if let Some(r) = rule {
            self.apply_inhibitor(r, kind, inhibited);
        } else {
            let rule_ids: Vec<RuleId> = self.rule_displays.keys().cloned().collect();
            for r in rule_ids {
                self.apply_inhibitor(&r, kind, inhibited);
            }
        }
    }

    /// Update one rule's per-kind inhibitor bookkeeping and, ONLY when the
    /// OR-derived effective bit actually changes, fan out
    /// [`Input::InhibitorChanged`] to that rule's displays.
    ///
    /// This is the change-dedup mechanism: a repeated `SetInhibited` for a
    /// kind that doesn't flip the effective bit is a pure no-op past the
    /// bookkeeping update — no display step, no [`Effect`], no `SinkCmd`.
    fn apply_inhibitor(&mut self, rule: &RuleId, kind: InhibitorKind, inhibited: bool) {
        let set = self.inhibitor_state.entry(rule.clone()).or_default();
        set.kinds.insert(kind, inhibited);
        let new_effective = set.kinds.values().any(|&engaged| engaged);
        let changed = new_effective != set.effective;
        if !changed {
            return;
        }
        set.effective = new_effective;

        let targets = self.rule_displays.get(rule).cloned().unwrap_or_default();
        for d in targets {
            self.step_one(&d, Input::InhibitorChanged(new_effective));
        }
    }

    #[allow(clippy::needless_pass_by_value)] // `rule` is dispatched by ref + clone below
    fn handle_pause(&mut self, rule: Option<&RuleId>, until: Option<Timestamp>) {
        let targets: Vec<DisplayId> = match rule {
            Some(r) => self.rule_displays.get(r).cloned().unwrap_or_default(),
            None => self
                .cfg
                .displays
                .iter()
                .map(|d| d.display.clone())
                .collect(),
        };
        if let Some(r) = rule {
            self.paused_rules.insert(r.clone());
        } else {
            // Global pause: pause every rule.
            for r in self.rule_displays.keys() {
                self.paused_rules.insert(r.clone());
            }
        }
        let until_tick = until.and_then(map_timestamp_to_tick);
        for d in targets {
            if self.machines.contains_key(&d) {
                self.paused_scopes.insert(d.clone(), rule.cloned());
            }
            self.step_one(&d, Input::Pause { until: until_tick });
        }
    }

    fn handle_resume(&mut self, rule: Option<&RuleId>) {
        let targets: Vec<DisplayId> = match rule {
            Some(r) => self.rule_displays.get(r).cloned().unwrap_or_default(),
            None => self
                .cfg
                .displays
                .iter()
                .map(|d| d.display.clone())
                .collect(),
        };
        if let Some(r) = &rule {
            self.paused_rules.remove(r);
        } else {
            self.paused_rules.clear();
        }
        for d in targets {
            self.step_one(&d, Input::Resume);
        }
    }

    /// Handle [`ControlMsg::EmergencyWake`]: pause every rule indefinitely
    /// via the existing pause fan-out, then spawn **one task per display**
    /// (genuine concurrency — not a serial loop) that calls
    /// [`CommandSink::wake_once`] on its executor.  Results aggregate into
    /// a [`EmergencyWakeReport`] the handler sends back through the
    /// oneshot.
    ///
    /// Parallelism is the entire point of the panic-recovery path: under
    /// the IPC 2-second budget a slow network wake on one display (Samsung
    /// Tizen, HA-passthrough) must not starve the remaining displays of
    /// their wake.  Spawning one task per `Arc<dyn CommandSink>`, then
    /// awaiting the `JoinHandle`s in the outer task, mirrors the
    /// `direct_hardware_fallback` shape in `dormantctl`.
    ///
    /// The wake execution runs **outside** the engine run loop so a
    /// stalled wake does not block other control messages.  The IPC
    /// server wraps this call in its own 2-second timeout (see
    /// `dormantd::ipc::handle_emergency_wake`); the `dormantctl` client
    /// falls back to direct-hardware construction when that window
    /// elapses.
    fn handle_emergency_wake(
        &mut self,
        reply: oneshot::Sender<EmergencyWakeReport>,
        accepted: Option<AcceptedOperation>,
    ) {
        // Pause every rule indefinitely — reuse the existing pause path so
        // the state-machine overlays route the same way as a normal
        // global pause.  Without this an absent-zone would re-trigger
        // blanking on the very next sensor event.
        self.handle_pause(None, None);

        // Snapshot executor handles so the spawned task does not hold a
        // borrow on `self`.  Per the spec, wake EVERY display the engine
        // owns — that includes manual-only displays (no rule bound).
        let lease = accepted
            .as_ref()
            .and_then(|operation| self.operation_registry.lease_accepted(operation));
        let executors: Vec<(DisplayId, Arc<dyn CommandSink>)> = self
            .executors
            .iter()
            .map(|(id, sink)| (id.clone(), Arc::clone(sink)))
            .collect();

        tracing::info!(
            event = "emergency_wake",
            display_count = executors.len(),
            "emergency-wake: paused all rules, dispatching one wake_once per display (concurrent)",
        );

        tokio::spawn(async move {
            let _lease = lease;
            // Spawn ALL per-display wake tasks up front, THEN await them.
            // Awaiting inside the same loop would force serial execution
            // and let one slow controller (Tizen, HA-passthrough) block
            // every other display under the IPC 2-second budget.
            let handles: Vec<(DisplayId, tokio::task::JoinHandle<Result<(), CmdFailure>>)> =
                executors
                    .into_iter()
                    .map(|(display_id, sink)| {
                        let handle = tokio::spawn(async move { sink.wake_once().await });
                        (display_id, handle)
                    })
                    .collect();

            let mut results: Vec<EmergencyWakeResult> = Vec::with_capacity(handles.len());
            for (display_id, handle) in handles {
                match handle.await {
                    Ok(Ok(())) => results.push(EmergencyWakeResult {
                        display: display_id,
                        ok: true,
                        error: None,
                    }),
                    Ok(Err(failure)) => {
                        tracing::warn!(
                            event = "emergency_wake_display_failed",
                            display_id = %display_id,
                            controller = failure.controller.as_str(),
                            error = %failure.error,
                            "emergency-wake: wake_once failed for one display",
                        );
                        results.push(EmergencyWakeResult {
                            display: display_id,
                            ok: false,
                            error: Some(failure.error),
                        });
                    }
                    Err(join_err) => {
                        // A spawned per-display task panicked. Log and
                        // continue, but keep a failed row so the report
                        // accounts for every display the daemon attempted.
                        tracing::warn!(
                            event = "emergency_wake_task_panicked",
                            display_id = %display_id,
                            error = %join_err,
                            "emergency-wake: spawned wake task panicked",
                        );
                        results.push(EmergencyWakeResult {
                            display: display_id,
                            ok: false,
                            error: Some(format!("spawned wake task panicked: {join_err}")),
                        });
                    }
                }
            }

            let report = EmergencyWakeReport {
                paused: true,
                operation_id: accepted.as_ref().map(|operation| operation.id),
                generation: accepted.as_ref().map(|operation| operation.generation),
                displays: results,
            };
            let _ = reply.send(report);
        });
    }

    /// Handle [`ControlMsg::Exercise`]: pause every rule that drives the
    /// target display, record the pre-exercise phase, run the blank → read
    /// → wake → read → restore sequence on the executor, and reply with an
    /// [`ExerciseReport`].
    ///
    /// The exercise runs **outside** the engine run loop (mirroring
    /// [`Self::handle_emergency_wake`]) so a slow wake or blank on the
    /// target display does not block other control messages.  The handler
    /// reads its inputs from `self`, snapshots everything it needs, and
    /// moves the executor handle into the spawned task via `Arc::clone` —
    /// the run loop keeps a `HashMap<DisplayId, Arc<dyn CommandSink>>` it
    /// can read, but cannot hand out mutable references, so the spawned
    /// task takes the cloned `Arc<dyn CommandSink>` and calls
    /// [`CommandSink::blank`] / [`CommandSink::wake`] /
    /// [`CommandSink::read_state`] directly.
    ///
    /// **Wake-path safety (cardinal rule)**: the restore step ALWAYS issues
    /// a final `wake()` if the pre-exercise phase was active or a
    /// blank-equivalent step if the pre-exercise phase was already a
    /// blanked-family phase.  Even if any earlier step panicked or errored
    /// mid-exercise, the restore step's blanket invocation of the wake path
    /// means an exercise cannot leave a panel dark.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::needless_pass_by_value)]
    fn handle_exercise(
        &mut self,
        target: DisplayId,
        reply: oneshot::Sender<ExerciseReport>,
        accepted: Option<AcceptedOperation>,
    ) {
        let generation = accepted.as_ref().map_or_else(
            || {
                self.observations
                    .as_ref()
                    .map_or(GenerationId(0), |(id, _)| *id)
            },
            |operation| operation.generation,
        );
        let current_generation = self.observations.as_ref().map_or(generation, |(id, _)| *id);
        let acquired = accepted
            .as_ref()
            .and_then(|operation| {
                self.operation_registry
                    .lease_accepted(operation)
                    .map(|lease| (operation.clone(), lease))
            })
            .or_else(|| {
                self.operation_registry
                    .try_acquire(current_generation, OperationKind::Exercise(target.clone()))
                    .ok()
            });
        let Some((acquired_operation, lease)) = acquired else {
            let _ = reply.send(ExerciseReport {
                display: target,
                pre_phase: "rejected".into(),
                operation_id: accepted.as_ref().map(|operation| operation.id),
                generation: accepted.as_ref().map(|operation| operation.generation),
                paused_rules: Vec::new(),
                steps: vec![ExerciseStep {
                    command: "reject".into(),
                    blank_mode: None,
                    returned_ok: false,
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Failed,
                    error: Some("E_OPERATION_IN_PROGRESS: display exercise already active".into()),
                }],
            });
            return;
        };
        let operation = acquired_operation;
        let operation_id = operation.id;
        let accepted_generation = operation.generation;
        // Snapshot the rules bound to this display so the spawned task can
        // un-pause them without holding a borrow on `self`.
        let rules_for_target: Vec<RuleId> = self
            .rule_displays
            .iter()
            .filter_map(|(rule, displays)| {
                if displays.contains(&target) {
                    Some(rule.clone())
                } else {
                    None
                }
            })
            .collect();

        // Pause every rule bound to this display — reuse the existing
        // pause path so the state-machine overlays route identically to a
        // normal operator pause.  Without this an absent-zone could re-
        // trigger blanking on the very next sensor event and fight the
        // exercise.
        for r in &rules_for_target {
            self.handle_pause(Some(r), None);
        }

        // Record pre-exercise phase (one of the literals from
        // [`DisplayStateMachine::phase_name`] — `active`, `grace`,
        // `blanking`, `blanked`, `waking`, `render_pending`, `staged`).
        let pre_phase = self
            .machines
            .get(&target)
            .map_or_else(|| "unknown".to_string(), |m| m.phase_name().to_string());

        // Pull the effective blank mode the display would normally be
        // blanked with — the exercise issues the same command the rules
        // engine would, so the test reads back the panel in the same
        // state the production path expects.
        let effective_mode = self
            .cfg
            .displays
            .iter()
            .find(|d| d.display == target)
            .map(|d| d.blank_mode);

        let Some(sink) = self.executors.get(&target).cloned() else {
            // No executor — surface an empty report and un-pause what we
            // paused (nothing for a display that has no executor at all,
            // but be defensive).
            for r in &rules_for_target {
                self.handle_resume(Some(r));
            }
            let _ = reply.send(ExerciseReport {
                display: target,
                pre_phase,
                operation_id: Some(operation_id),
                generation: Some(accepted_generation),
                paused_rules: Vec::new(),
                steps: vec![ExerciseStep {
                    command: "no_executor".into(),
                    blank_mode: None,
                    returned_ok: false,
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Failed,
                    error: Some("E_DISPLAY_IO: no executor registered".into()),
                }],
            });
            return;
        };

        let paused_count = rules_for_target.len();
        tracing::info!(
            event = "control_path_exercise",
            display = %target,
            pre_phase = %pre_phase,
            paused_rules = paused_count,
            effective_mode = ?effective_mode,
            "exercise: paused rule(s); running blank → read → wake → read → restore",
        );

        let rules_to_resume: Vec<RuleId> = rules_for_target.clone();

        // Clone the internal results sender so the spawned task can
        // guarantee the rule-pause window is released ENGINE-SIDE — the
        // IPC layer may be gone (caller dropped the receiver, or hit
        // EXERCISE_IPC_TIMEOUT) by the time the sequence completes, so
        // routing the resume through the run loop's &mut self is the
        // only path that doesn't depend on the IPC caller still being
        // alive.  This mirrors how `process_effect` clones `results_tx`
        // for spawned blank/wake tasks.
        let results_tx = self.results_tx.clone();
        let wake_settle = self.cfg.doctor_wake_settle;

        tokio::spawn(async move {
            let lease = lease;
            let pause_guard = ExercisePauseGuard {
                results_tx: results_tx.clone(),
                rules: Some(rules_to_resume.clone()),
                operation_id,
                generation: accepted_generation,
            };
            let mut report = match run_supervised_exercise_cancellable(
                Some(&lease),
                Arc::clone(&sink),
                effective_mode,
                pre_phase,
                rules_to_resume.clone(),
                target.clone(),
                wake_settle,
            )
            .await
            {
                Ok(report) => report,
                Err(()) => ExerciseReport {
                    display: target,
                    pre_phase: "unknown".into(),
                    operation_id: Some(operation_id),
                    generation: Some(accepted_generation),
                    paused_rules: rules_to_resume.clone(),
                    steps: vec![ExerciseStep {
                        command: "panic".into(),
                        blank_mode: None,
                        returned_ok: false,
                        state_before: None,
                        state_after: None,
                        verdict: ExerciseVerdict::Failed,
                        error: Some("E_EXERCISE_PANIC: exercise task panicked".into()),
                    }],
                },
            };
            report.operation_id = Some(operation_id);
            report.generation = Some(accepted_generation);

            tracing::info!(
                event = "control_path_exercise",
                display = %report.display,
                step_count = report.steps.len(),
                paused_rules = report.paused_rules.len(),
                "exercise: complete; releasing rule pause via results channel",
            );
            // Unconditional engine-side resume: the run loop's
            // `handle_internal_result` arm fires on the very next drain,
            // independent of whether `reply.send` succeeds.  A timed-out
            // or disconnected IPC caller can no longer strand a paused
            // rule.  Empty Vec is a no-op (manual-only display path).
            drop(pause_guard);

            // `reply.send` is best-effort: the caller may have timed out
            // or disconnected, in which case the report is dropped on the
            // floor — the engine's rule pause was already released above,
            // which is the load-bearing invariant.
            let _ = reply.send(report);
        });
    }

    /// Step a single display machine and process its effects.
    ///
    /// Feeds [`Input::OwnershipChanged`] before the requested input so the
    /// machine's `owned` flag always reflects the gate's current verdict.
    fn step_one(&mut self, display: &DisplayId, input: Input) {
        let now = Tick::now();
        self.feed_ownership(display, now);
        self.step_machine(display, input, now);
    }

    /// Step a machine, forward a real phase change to diagnostics, then run
    /// the resulting effects. Observations deliberately remain non-blocking.
    fn step_machine(&mut self, display: &DisplayId, input: Input, now: Tick) {
        let Some(machine) = self.machines.get_mut(display) else {
            return;
        };
        let was_paused = machine.overlays().paused.is_some();
        let (effects, transition) = machine.step_with_transition(input, now);
        let is_paused = machine.overlays().paused.is_some();
        if let Some((old_phase, new_phase)) = transition {
            self.emit_phase_transition(display, old_phase, new_phase);
        }
        if was_paused != is_paused {
            let rule = self.paused_scopes.get(display).cloned().flatten();
            self.emit_pause_changed(display, is_paused, rule);
            if !is_paused {
                self.paused_scopes.remove(display);
            }
        }
        for effect in effects {
            self.process_effect(display, effect);
        }
    }

    fn emit_pause_changed(&self, display: &DisplayId, paused: bool, rule: Option<RuleId>) {
        let _ = self.event_tx.send(DaemonEvent::PauseChanged {
            display: display.clone(),
            paused,
            rule,
        });
    }

    fn emit_phase_transition(
        &self,
        display_id: &DisplayId,
        old_phase: crate::state_machine::Phase,
        new_phase: crate::state_machine::Phase,
    ) {
        let Some((generation, hub)) = &self.observations else {
            return;
        };
        // A multi-rule display reports its lexicographically-smallest rule id:
        // deterministic single-owner reporting keeps this diagnostic shape simple,
        // and multi-rule displays are rare.
        let rule_id = self
            .rule_displays
            .iter()
            .filter(|(_, displays)| displays.contains(display_id))
            .map(|(rule_id, _)| rule_id.clone())
            .min();
        hub.emit(DaemonObservation::DisplayPhaseChanged {
            generation: *generation,
            rule_id,
            display_id: display_id.clone(),
            old_phase,
            new_phase,
        });
    }

    /// Consult the [`OwnershipGate`] for `display` and, if the verdict
    /// differs from the last-fed value, feed
    /// [`Input::OwnershipChanged`] to the state machine.
    ///
    /// Processes any effects the machine produced (e.g. teardown-render on
    /// ownership-yield). When the gate returns the same value as last time,
    /// this is a no-op. F10: an in-flight claim's expected flip
    /// (a `false`-direction verdict) is suppressed — the rules
    /// engine does NOT feed `Input::OwnershipChanged(false)` to
    /// the state machine while the claim suppression deadline is
    /// in the future. `true`-direction (re-acquire) flips still
    /// feed normally.
    fn feed_ownership(&mut self, display: &DisplayId, now: Tick) {
        let owns = self.ownership.owns(display);
        if self.last_owned.get(display) == Some(&owns) {
            return;
        }
        // F10: a foreign ownership loss during an in-flight claim
        // is the claim's expected flip. Suppress the loss reaction.
        if !owns
            && let Some(deadline) = self.claim_suppression.get(display).copied()
            && now.0 < deadline
        {
            // Track the suppressed verdict so a later no-op
            // doesn't refire the feed once the suppression lifts —
            // we re-evaluate at the next non-suppressed tick.
            self.last_owned.insert(display.clone(), true);
            return;
        }
        self.last_owned.insert(display.clone(), owns);
        self.step_machine(display, Input::OwnershipChanged(owns), now);
    }

    fn send_snapshot(&self, tx: oneshot::Sender<StateSnapshot>) {
        let now_sys = std::time::SystemTime::now();
        let sensors = self
            .cfg
            .sensors
            .iter()
            .map(|scfg| {
                let (state, last_at) = self
                    .zone_engine
                    .sensor_states()
                    .get(&scfg.sensor)
                    .copied()
                    .unwrap_or((SensorState::Unavailable, Timestamp(now_sys)));
                let secs_ago = now_sys
                    .duration_since(last_at.0)
                    .unwrap_or(Duration::ZERO)
                    .as_secs();
                SensorSnapshot {
                    id: scfg.sensor.0.clone(),
                    state,
                    last_seen_secs_ago: secs_ago,
                    reported: self.reported.contains(&scfg.sensor),
                }
            })
            .collect();
        let zones = self
            .zone_engine
            .known_zone_ids()
            .map(|zid| ZoneSnapshot {
                id: zid.0.clone(),
                present: self.zone_engine.is_present(zid),
            })
            .collect();
        let mut displays: Vec<(String, DisplaySnapshot)> = Vec::new();
        for dcfg in &self.cfg.displays {
            if let Some(m) = self.machines.get(&dcfg.display) {
                let controllers = self
                    .executors
                    .get(&dcfg.display)
                    .map(|exec| exec.controller_health())
                    .unwrap_or_default();
                let coordination = self
                    .coordination
                    .as_ref()
                    .and_then(|handle| handle.snapshot().remove(&dcfg.display));
                let (scope, owned, observed_input_code, panel_state) = match coordination {
                    Some(record) => (
                        DisplayScope::Shared,
                        record.owned,
                        record.input_code,
                        record.panel_state,
                    ),
                    None => (DisplayScope::Private, true, None, None),
                };
                displays.push((
                    dcfg.display.0.clone(),
                    DisplaySnapshot {
                        phase: m.phase_name().to_string(),
                        inhibited: m.overlays().inhibited,
                        paused: m.overlays().paused.is_some(),
                        cmd_gen: m.cmd_gen(),
                        scope,
                        owned,
                        observed_input_code,
                        panel_state,
                        controllers,
                        wake_attempts: self.wake_attempts.get(&dcfg.display).copied().unwrap_or(0),
                        last_blank_failed: self.last_blank_failed.contains(&dcfg.display),
                        stage: m.current_stage().map(|(idx, kind)| StageInfo { idx, kind }),
                    },
                ));
            }
        }
        let _ = tx.send(StateSnapshot {
            sensors,
            zones,
            displays,
            pending_reload: self.pending_reload.clone(),
            rollback: self.rollback.clone(),
            kvm: self.kvm.clone(),
            wear_sampling_status: None,
        });
    }

    // ── Internal: results from spawned dispatch tasks ───────────────────────

    fn handle_internal_result(&mut self, res: InternalResult, now: Tick) {
        match res {
            InternalResult::Blank {
                display,
                r#gen,
                result,
            } => {
                // Move-order pin (spec F4): capture the folded failure
                // BEFORE `result` is moved into `machine.step` below —
                // precedent: `let ok = result.is_ok();` in the Wake arm.
                let failure: Option<CmdFailure> = result.as_ref().err().cloned();
                self.step_machine(&display, Input::BlankResult { r#gen, result }, now);
                match failure {
                    Some(f) => {
                        self.last_blank_failed.insert(display.clone());
                        let _ = self.event_tx.send(DaemonEvent::BlankFailure {
                            display,
                            controller: f.controller,
                            detail: f.error,
                        });
                    }
                    None => {
                        if self.last_blank_failed.remove(&display) {
                            let _ = self.event_tx.send(DaemonEvent::BlankRecovered { display });
                        }
                    }
                }
            }
            InternalResult::Wake {
                display,
                r#gen,
                result,
            } => {
                let ok = result.is_ok();
                self.step_machine(&display, Input::WakeResult { r#gen, result }, now);
                if ok {
                    if let Some(n) = self.wake_attempts.remove(&display)
                        && n > 0
                    {
                        let _ = self.event_tx.send(DaemonEvent::WakeRecovered {
                            display,
                            attempts: n,
                        });
                    }
                } else {
                    let attempt = self.wake_attempts.entry(display.clone()).or_insert(0);
                    *attempt = attempt.saturating_add(1);
                    let _ = self.event_tx.send(DaemonEvent::WakeRetry {
                        display,
                        attempt: *attempt,
                    });
                }
            }
            InternalResult::Render {
                display,
                r#gen,
                result,
            } => {
                self.step_machine(&display, Input::RenderResult { r#gen, result }, now);
            }
            InternalResult::ExerciseResume {
                rules,
                operation_id,
                generation,
            } => {
                let current_generation = self
                    .observations
                    .as_ref()
                    .map_or(GenerationId(0), |(id, _)| *id);
                if !operation_completion_matches_generation(generation, current_generation) {
                    tracing::warn!(
                        event = "operation_completion_discarded",
                        op_id = operation_id,
                        accepted_gen = generation.0,
                        current_gen = current_generation.0,
                        "discarding operation completion from an old generation",
                    );
                    return;
                }
                // Off-run-loop exercise sequence completed (possibly
                // successfully, possibly with the IPC caller having
                // timed out or dropped its receiver).  Resume every
                // rule the exercise paused so the engine can resume
                // blanking the moment the result is processed.
                for rule in &rules {
                    self.handle_resume(Some(rule));
                }
            }
        }
    }

    // ── Internal: timers ────────────────────────────────────────────────────

    /// Find the effective `input_wake_hold` for a display.
    ///
    /// Returns `None` if no rule drives this display or if any driving zone
    /// is currently present.  When every driving zone is vacant, returns the
    /// shortest matching hold so the display re-evaluates its vacancy state at
    /// the earliest deadline.
    fn effective_input_wake_hold(&self, display: &DisplayId) -> Option<Duration> {
        let mut effective: Option<Duration> = None;
        for rule in &self.cfg.rules {
            if rule.displays.contains(display) {
                let present = self.zone_engine.is_present(&rule.zone).unwrap_or(true); // unknown = present (fail-safe)
                if present {
                    // At least one driving zone is present — no hold.
                    return None;
                }
                // The shortest hold bounds how long stale vacancy information
                // can suppress a re-check when several rules share a display.
                effective = Some(
                    effective.map_or(rule.input_wake_hold, |hold| hold.min(rule.input_wake_hold)),
                );
            }
        }
        // Display is not driven by any rule (e.g. manual-only).
        effective
    }

    fn compute_sweep_period(&self) -> Duration {
        let min_stale = self
            .cfg
            .sensors
            .iter()
            .map(|s| s.stale_timeout)
            .min()
            .unwrap_or(Duration::from_secs(60));
        let half = min_stale / 2;
        if half < Duration::from_secs(1) {
            Duration::from_secs(1)
        } else {
            half
        }
    }

    fn fire_due_timers(&mut self, now: Tick) {
        // Pop every entry whose deadline is <= now.
        while let Some(Reverse(top)) = self.timers.peek() {
            if top.0.0 <= now.0 {
                let Reverse((deadline, timer_entry)) = self.timers.pop().expect("peeked");
                match timer_entry {
                    TimerEntry::DisplayTick(display) => {
                        self.feed_ownership(&display, deadline);
                        self.step_machine(&display, Input::Tick, deadline);
                    }
                    TimerEntry::DisplayStageTick(display, stage_gen) => {
                        self.feed_ownership(&display, deadline);
                        self.step_machine(
                            &display,
                            Input::StageTick { r#gen: stage_gen },
                            deadline,
                        );
                    }
                    TimerEntry::HoldExpiry(sensor_id) => {
                        self.fire_hold_expiry(&sensor_id, deadline);
                    }
                    TimerEntry::InputWakeHoldExpiry(display) => {
                        self.fire_input_wake_hold_expiry(&display, deadline);
                    }
                }
            } else {
                break;
            }
        }
        let _ = now; // accepted parameter for symmetry
    }

    fn fire_hold_expiry(&mut self, sensor_id: &SensorId, now: Tick) {
        // Drop the entry if the hold was re-armed past this deadline (a
        // second Present pushed a later `armed_until` and a later timer).
        let Some(hold) = self.holds.get_mut(sensor_id) else {
            return;
        };
        // Disarmed by an Unavailable event (or never armed) — nothing to
        // do, and any stray pending_absent should be discarded.
        let Some(armed_until) = hold.armed_until else {
            hold.pending_absent = None;
            return;
        };
        if now < armed_until {
            // Stale timer — a newer Present re-armed the hold past this
            // deadline. Drop without acting; the newer timer will fire
            // when its own deadline elapses.
            return;
        }
        let pending = hold.pending_absent.take();
        hold.armed_until = None;
        if let Some(ev) = pending {
            self.handle_presence_event(ev);
        }
    }

    /// Input-wake hold expiry (issue #125): clear the hold flag and re-enter
    /// the normal grace path — never a direct blank.  A stale timer (hold
    /// already cleared by presence or a newer `InputWake`) is silently dropped.
    fn fire_input_wake_hold_expiry(&mut self, display: &DisplayId, now: Tick) {
        // Guard against stale timers: a second `InputWake` during the hold
        // pushes a later deadline into the map, but the OLD heap timer still
        // fires.  Check the stored deadline before acting — mirroring
        // `fire_hold_expiry`'s `now < armed_until` guard.
        let Some(&stored_deadline) = self.input_wake_holds.get(display) else {
            // Hold already cleared (presence returned, display removed).
            return;
        };
        if stored_deadline > now.0 {
            // Stale timer — a newer InputWake re-armed the hold past this
            // expiry.  Drop without acting; the newer timer will fire.
            return;
        }
        // Timer is current — safe to remove and act.
        self.input_wake_holds.remove(display);
        if let Some(machine) = self.machines.get_mut(display) {
            machine.set_input_wake_hold_active(false);
        }
        self.feed_ownership(display, now);
        // Presence may have changed while the hold was active without an edge
        // reaching this engine.  Never re-enter Grace from a display that is
        // currently driven by an occupied or unknown zone.
        if self.effective_input_wake_hold(display).is_some() {
            self.step_machine(display, Input::ZonePresent(false), now);
        }
    }

    // ── Internal: stale sensor sweep ────────────────────────────────────────

    fn sweep_stale_sensors(&mut self) {
        let v_now = tokio::time::Instant::now();
        // Snapshot sensor config list so the immutable borrow on `self.cfg`
        // ends before the mutable borrow on `self` for handle_presence_event.
        let sensors: Vec<(SensorId, Duration)> = self
            .cfg
            .sensors
            .iter()
            .map(|s| (s.sensor.clone(), s.stale_timeout))
            .collect();
        for (sensor_id, stale_timeout) in sensors {
            let Some((state, _)) = self.zone_engine.sensor_states().get(&sensor_id).copied() else {
                continue;
            };
            if state == SensorState::Unavailable {
                continue;
            }
            // A source-asserted `online` availability edge acts as a LEASE
            // bounded by `stale_timeout`. Below the lease, topic silence is
            // "state unchanged" (no sweep). When the lease expires, clear the
            // marker and fall through to the normal stale path so the next
            // sweep iteration fires `Unavailable`. Issue #205: an `online`
            // retained sensor that then goes silent forever must still go
            // Unavailable, otherwise a dead connection whose last broker-side
            // state was `online` preserves stale presence forever.
            if let Some(lease_start) = self.availability_online.get(&sensor_id).copied() {
                if v_now.saturating_duration_since(lease_start) > stale_timeout {
                    self.availability_online.remove(&sensor_id);
                    // Fall through to the normal stale path.
                } else {
                    continue;
                }
            }
            // Use virtual time for the elapsed comparison so paused tests
            // can drive minutes in milliseconds.
            let Some(last_v) = self.sensor_last_seen_virtual.get(&sensor_id).copied() else {
                continue;
            };
            let elapsed = v_now.saturating_duration_since(last_v);
            if elapsed > stale_timeout {
                let ev = PresenceEvent::new(
                    sensor_id,
                    SensorState::Unavailable,
                    Timestamp(std::time::SystemTime::now()),
                );
                self.handle_presence_event(ev);
            }
        }
    }

    // ── Internal: effect dispatch ───────────────────────────────────────────

    #[allow(clippy::needless_pass_by_value)] // consumed field-by-field in the match below
    fn process_effect(&mut self, display_id: &DisplayId, effect: Effect) {
        match effect {
            Effect::IssueBlank { r#gen, mode } => {
                if let Some(sink) = self.executors.get(display_id) {
                    let sink = Arc::clone(sink);
                    let display = display_id.clone();
                    let tx = self.results_tx.clone();
                    tokio::spawn(async move {
                        let result = sink.blank(mode).await;
                        let _ = tx.send(InternalResult::Blank {
                            display,
                            r#gen,
                            result,
                        });
                    });
                }
            }
            Effect::IssueWake { r#gen } => {
                if let Some(sink) = self.executors.get(display_id) {
                    let sink = Arc::clone(sink);
                    let display = display_id.clone();
                    let tx = self.results_tx.clone();
                    tokio::spawn(async move {
                        let result = sink.wake().await;
                        let _ = tx.send(InternalResult::Wake {
                            display,
                            r#gen,
                            result,
                        });
                    });
                }
            }
            Effect::ScheduleTickAt(tick) => {
                self.timers
                    .push(Reverse((tick, TimerEntry::DisplayTick(display_id.clone()))));
            }
            Effect::ScheduleStageTickAt { r#gen, at } => {
                self.timers.push(Reverse((
                    at,
                    TimerEntry::DisplayStageTick(display_id.clone(), r#gen),
                )));
            }
            Effect::ShowRender { r#gen, idx, kind } => {
                if let Some(sink) = self.render_sinks.get(display_id) {
                    let sink = Arc::clone(sink);
                    let display = display_id.clone();
                    let tx = self.results_tx.clone();
                    tokio::spawn(async move {
                        let result = sink.show(r#gen, idx, kind).await;
                        let _ = tx.send(InternalResult::Render {
                            display,
                            r#gen,
                            result,
                        });
                    });
                } else {
                    // No render backend — render stage fails fall-through
                    // so the machine never wedges in RenderPending.
                    //
                    let display = display_id.clone();
                    let tx = self.results_tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(InternalResult::Render {
                            display,
                            r#gen,
                            result: Err(CmdFailure {
                                controller: "render-none".into(),
                                error: "E_RENDER_UNAVAILABLE: no render backend".into(),
                            }),
                        });
                    });
                }
            }
            Effect::TeardownRender { r#gen } => {
                if let Some(sink) = self.render_sinks.get(display_id) {
                    let sink = Arc::clone(sink);
                    tokio::spawn(async move {
                        sink.teardown(r#gen).await;
                    });
                }
                // No-op when no sink — teardown is idempotent.
            }
            Effect::LogTransition { from: _, to, cause } => {
                tracing::info!(
                    event = "display_phase",
                    display_id = %display_id,
                    phase = %to,
                    cause = %cause,
                    "display phase transition"
                );
                let _ = self.event_tx.send(DaemonEvent::DisplayPhase {
                    display: display_id.clone(),
                    phase: to.to_string(),
                    cause: cause.to_string(),
                });
            }
        }
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Map a wall-clock `Timestamp` to a monotonic `Tick`.  Returns `None` if the
/// timestamp is in the past and would map to a negative offset (clamped to 0).
fn map_timestamp_to_tick(ts: Timestamp) -> Option<Tick> {
    let now_sys = std::time::SystemTime::now();
    let now_mono = std::time::Instant::now();
    let delta = ts.0.duration_since(now_sys).unwrap_or(Duration::ZERO);
    now_mono.checked_add(delta).map(Tick)
}

fn to_tokio_instant(t: std::time::Instant) -> tokio::time::Instant {
    tokio::time::Instant::from_std(t)
}

// ── Exercise sequence (helper, off the run loop) ──────────────────────────────

/// Per-`read_state` budget.  A hung transport (Samsung network partition,
/// DDC bus lockup) would otherwise block the spawned task indefinitely —
/// leaking the task, holding the sink `Arc`, and stalling the rule-pause
/// release.  3s is conservative: healthy DDC VCP reads return in
/// ~200 ms, Samsung REST `PowerState` reads return in ~500 ms.
const READ_STATE_TIMEOUT: Duration = Duration::from_secs(3);

/// Wrap a `read_state()` call with the per-read budget.  A timeout
/// returns `None`, which the caller renders as `Unconfirmable` rather
/// than a hung-read `Failed` (the panel may have moved; we just can't
/// observe it in time).
async fn bounded_read_state(sink: Arc<dyn CommandSink>) -> Option<PanelState> {
    match tokio::time::timeout(READ_STATE_TIMEOUT, sink.read_state()).await {
        Ok(state) => state,
        Err(_elapsed) => {
            tracing::warn!(
                event = "control_path_exercise",
                timeout_s = READ_STATE_TIMEOUT.as_secs(),
                "read_state() exceeded per-read budget; treating as Unconfirmable",
            );
            None
        }
    }
}

/// Read the panel state after `wake()` returns, retrying once after a
/// settle window if the first read is absent or still non-`On`.
///
/// Some panels haven't finished powering on by the time the wake command
/// returns — an immediate readback can report absent or a stale non-`On`
/// state even though the wake will shortly succeed.  Rather than false-
/// negative the wake step, this gives the panel one settle window
/// (`daemon.doctor_wake_settle`) and one bounded retry read before the
/// caller classifies the step.  No delay when the first read already
/// confirms `On`.
///
/// Classifies from the best/last observation: the retry read is the
/// freshest data and wins when present; falls back to the first read
/// (which may itself be a non-`On` observation, still more informative
/// than nothing) when the retry produced no readback.
async fn read_after_wake_with_settle(
    sink: &Arc<dyn CommandSink>,
    display: &DisplayId,
    wake_settle: Duration,
) -> Option<PanelState> {
    let first_read = bounded_read_state(sink.clone()).await;
    let confirmed_on = matches!(
        first_read.as_ref().and_then(|s| s.power),
        Some(PowerState::On)
    );
    if confirmed_on {
        return first_read;
    }

    let display_for_log = display;
    tracing::info!(
        event = "control_path_exercise",
        display = %display_for_log,
        post_wake_retry = true,
        settle_ms = wake_settle.as_millis(),
        "exercise: first post-wake read absent or non-On; retrying after panel settle",
    );
    tokio::time::sleep(wake_settle).await;
    let retry_read = bounded_read_state(sink.clone()).await;
    retry_read.or(first_read)
}

/// Drive the [`ControlMsg::Exercise`] sequence against `sink` and return the
/// aggregated [`ExerciseReport`].
///
/// Factored out of [`RulesEngine::handle_exercise`] so the engine-side
/// handler stays short (mirrors how `handle_emergency_wake` itself was
/// extracted).  Runs **off** the engine run loop — the executor handle is
/// `Arc<dyn CommandSink>`, safe to share with a spawned task.
///
/// **Wake-path safety (cardinal rule)**: this function MUST end with a
/// panel that is awake when `pre_phase` was an active phase.  The restore
/// step issues a defensive `wake_once()` for active pre-phases so a
/// partially-failed exercise cannot strand a display dark.
async fn run_exercise_sequence(
    sink: &Arc<dyn CommandSink>,
    effective_mode: Option<BlankMode>,
    pre_phase: String,
    paused_rules: Vec<RuleId>,
    display: DisplayId,
    wake_settle: Duration,
) -> ExerciseReport {
    let mode = effective_mode.unwrap_or(BlankMode::PowerOff);
    let mut steps: Vec<ExerciseStep> = Vec::new();

    // 1. Baseline read — None means the executor has no readback (or hit
    //    the per-read budget — both surface the same `Unconfirmable`).
    let baseline = bounded_read_state(sink.clone()).await;
    steps.push(ExerciseStep {
        command: "read".into(),
        blank_mode: None,
        returned_ok: true,
        state_before: None,
        state_after: baseline.clone(),
        verdict: ExerciseVerdict::Unconfirmable,
        error: None,
    });

    // 2. blank(mode) → read.  Confirmed iff the panel state moved from
    //    baseline; Failed iff it didn't (panel didn't move despite the
    //    command returning Ok).
    let blank_result = sink.blank(mode).await;
    let state_after_blank = bounded_read_state(sink.clone()).await;
    let blank_verdict = blank_verdict(&blank_result, baseline.as_ref(), state_after_blank.as_ref());
    steps.push(ExerciseStep {
        command: "blank".into(),
        blank_mode: Some(mode),
        returned_ok: blank_result.is_ok(),
        state_before: baseline.clone(),
        state_after: state_after_blank.clone(),
        verdict: blank_verdict,
        error: blank_result.err().map(|f| f.error),
    });

    // 3. wake() → read.  Confirmed iff the panel state returned to the
    //    ORIGINAL baseline — the wake-path restoration check.  A panel
    //    that hasn't finished powering on yet may report absent or a
    //    non-`On` readback immediately after the wake command returns —
    //    `read_after_wake_with_settle` gives it exactly one bounded
    //    retry after `wake_settle` before classifying.
    let wake_result = sink.wake().await;
    let state_after_wake = read_after_wake_with_settle(sink, &display, wake_settle).await;
    let wake_verdict = restore_verdict(
        &wake_result,
        baseline.as_ref(),
        state_after_blank.as_ref(),
        state_after_wake.as_ref(),
    );
    let state_before_restore = state_after_wake.clone();
    steps.push(ExerciseStep {
        command: "wake".into(),
        blank_mode: None,
        returned_ok: wake_result.is_ok(),
        state_before: state_after_blank,
        state_after: state_after_wake,
        verdict: wake_verdict,
        error: wake_result.err().map(|f| f.error),
    });

    // 4. RESTORE — wake-path-sacred.  If the pre-exercise phase was a
    //    blanked-family phase, re-issue the blank so the display returns to
    //    its starting state.  Otherwise the wake in step 3 already left it
    //    awake — still send a defensive wake so a step-3 silently-no-op'd
    //    wake (e.g. unreachable TV) never strands the panel dark.
    let restore_step = if is_blanked_family_phase(&pre_phase) {
        let result = sink.blank(mode).await;
        let after = bounded_read_state(sink.clone()).await;
        // Restore-via-blank: pre-restore state was post-wake; we expect
        // after == post-wake (the panel stays in a blanked-family state)
        // OR after == baseline (the panel came back awake).  Either way
        // the command succeeding is the operationally interesting fact —
        // mark `Confirmed` on success.
        let verdict = if result.is_ok() {
            ExerciseVerdict::Confirmed
        } else {
            ExerciseVerdict::Failed
        };
        ExerciseStep {
            command: "restore".into(),
            blank_mode: Some(mode),
            returned_ok: result.is_ok(),
            state_before: state_before_restore,
            state_after: after,
            verdict,
            error: result.err().map(|f| f.error),
        }
    } else {
        let result = sink.wake_once().await;
        let after = bounded_read_state(sink.clone()).await;
        // Defensive restore-wake verdict: the wake step already verified
        // the panel state returned to baseline.  The defensive wake's
        // only job is to guarantee the panel is awake — its own state
        // movement is incidental.
        // - No readback → Unconfirmable (honest: we can't observe whether
        //   the defensive wake actually did anything).
        // - Command succeeded → Confirmed (the panel is awake).
        // - Command failed → Failed (the defensive wake errored AND a
        //   step-3 silently-no-op'd wake is suspected).
        let verdict = match (&result, after.is_none()) {
            (Err(_), _) => ExerciseVerdict::Failed,
            (Ok(()), true) => ExerciseVerdict::Unconfirmable,
            (Ok(()), false) => ExerciseVerdict::Confirmed,
        };
        ExerciseStep {
            command: "restore".into(),
            blank_mode: None,
            returned_ok: result.is_ok(),
            state_before: state_before_restore,
            state_after: after,
            verdict,
            error: result.err().map(|f| f.error),
        }
    };
    steps.push(restore_step);

    ExerciseReport {
        display,
        pre_phase,
        operation_id: None,
        generation: None,
        paused_rules,
        steps,
    }
}

async fn wake_after_exercise_panic(sink: &Arc<dyn CommandSink>) {
    let _ = sink.wake_once().await;
}

#[cfg(test)]
async fn run_supervised_exercise(
    sink: Arc<dyn CommandSink>,
    effective_mode: Option<BlankMode>,
    pre_phase: String,
    paused_rules: Vec<RuleId>,
    display: DisplayId,
    wake_settle: Duration,
) -> Result<ExerciseReport, ()> {
    run_supervised_exercise_cancellable(
        None,
        sink,
        effective_mode,
        pre_phase,
        paused_rules,
        display,
        wake_settle,
    )
    .await
}

async fn run_supervised_exercise_cancellable(
    lease: Option<&OperationLease>,
    sink: Arc<dyn CommandSink>,
    effective_mode: Option<BlankMode>,
    pre_phase: String,
    paused_rules: Vec<RuleId>,
    display: DisplayId,
    wake_settle: Duration,
) -> Result<ExerciseReport, ()> {
    let task_sink = Arc::clone(&sink);
    let task = tokio::spawn(async move {
        run_exercise_sequence(
            &task_sink,
            effective_mode,
            pre_phase,
            paused_rules,
            display,
            wake_settle,
        )
        .await
    });
    let result = if let Some(lease) = lease {
        tokio::select! {
            result = task => result,
            () = lease.cancelled() => {
                let _ = sink.wake_once().await;
                return Err(());
            }
        }
    } else {
        task.await
    };
    match result {
        Ok(report) => Ok(report),
        Err(error) => {
            tracing::error!(event = "exercise_task_panicked", error = %error, "exercise task failed; forcing wake");
            wake_after_exercise_panic(&sink).await;
            Err(())
        }
    }
}

/// Verdict for the blank step: did the panel state move from baseline?
///
/// Order matters: a command error is `Failed` regardless of readback
/// availability (the command itself failed), and a missing readback is
/// `Unconfirmable` even when the command returned `Ok`.  Only when the
/// command succeeded AND both observations are present do we compare
/// states — the case the feature exists to catch is "Ok but the panel
/// didn't move", which is `Failed`.
fn blank_verdict(
    result: &Result<(), CmdFailure>,
    baseline: Option<&PanelState>,
    after: Option<&PanelState>,
) -> ExerciseVerdict {
    if result.is_err() {
        return ExerciseVerdict::Failed;
    }
    if baseline.is_none() || after.is_none() {
        return ExerciseVerdict::Unconfirmable;
    }
    if baseline == after {
        ExerciseVerdict::Failed
    } else {
        ExerciseVerdict::Confirmed
    }
}

/// Verdict for the wake / restore step: did the wake command actually
/// move the panel AND restore it to baseline?
///
/// Requires THREE observations: baseline, post-blank (so we can tell
/// whether the wake itself changed anything), and post-wake.  The
/// "panel never moved at all" failure shape — where baseline ==
/// post-blank == post-wake — would otherwise score Confirmed on a wake
/// verdict that did nothing; requiring the wake to have moved the
/// panel catches that case as `Failed`.
fn restore_verdict(
    result: &Result<(), CmdFailure>,
    baseline: Option<&PanelState>,
    post_blank: Option<&PanelState>,
    post_wake: Option<&PanelState>,
) -> ExerciseVerdict {
    if result.is_err() {
        return ExerciseVerdict::Failed;
    }
    if baseline.is_none() || post_blank.is_none() || post_wake.is_none() {
        return ExerciseVerdict::Unconfirmable;
    }
    // Wake should have moved the panel from its post-blank state.
    if post_wake == post_blank {
        return ExerciseVerdict::Failed;
    }
    // Wake should have returned the panel to baseline.
    if post_wake != baseline {
        return ExerciseVerdict::Failed;
    }
    ExerciseVerdict::Confirmed
}

/// True for phases where the display is blanked (or in the process of
/// being blanked / staged) — the restore step must re-issue the blank so
/// the display returns to its pre-exercise state.
fn is_blanked_family_phase(phase: &str) -> bool {
    matches!(phase, "blanked" | "blanking" | "staged" | "render_pending")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{RecordingSink, SinkCmd};
    use crate::traits::{PanelState, PowerState};

    #[tokio::test]
    async fn operation_registry_rejects_second_exercise_same_display() {
        let registry = OperationRegistry::default();
        let generation = GenerationId(7);
        let (_accepted, lease) = registry
            .try_acquire(
                generation,
                OperationKind::Exercise(DisplayId("oled".into())),
            )
            .expect("first operation accepted");
        assert!(
            registry
                .try_acquire(
                    generation,
                    OperationKind::Exercise(DisplayId("oled".into()))
                )
                .is_err()
        );
        drop(lease);
        assert!(
            registry
                .try_acquire(
                    generation,
                    OperationKind::Exercise(DisplayId("oled".into()))
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn operation_registry_cancellation_marks_generation_operations() {
        let registry = OperationRegistry::default();
        let generation = GenerationId(9);
        let (_accepted, lease) = registry
            .try_acquire(generation, OperationKind::EmergencyWake)
            .expect("operation accepted");
        registry.cancel_generation(generation);
        assert!(lease.is_cancelled());
        drop(lease);
        registry.wait_generation_empty(generation).await;
    }

    #[tokio::test]
    async fn panic_mid_exercise_wakes_and_restores_pause() {
        let fake = Arc::new(crate::fakes::ExerciseSink::new());
        let sink: Arc<dyn CommandSink> = fake.clone();
        fake.panic_once_on("blank");
        assert!(
            run_supervised_exercise(
                sink,
                Some(BlankMode::PowerOff),
                "active".into(),
                vec![RuleId("office".into())],
                DisplayId("oled".into()),
                Duration::ZERO,
            )
            .await
            .is_err()
        );
        assert!(matches!(fake.log().last(), Some(SinkCmd::Wake)));
    }

    #[tokio::test]
    async fn cancelled_exercise_wakes_and_restores_pause() {
        let registry = OperationRegistry::default();
        let generation = GenerationId(11);
        let (_accepted, lease) = registry
            .try_acquire(
                generation,
                OperationKind::Exercise(DisplayId("oled".into())),
            )
            .expect("operation accepted");
        registry.cancel_generation(generation);
        assert!(lease.is_cancelled());
        let (results_tx, mut results_rx) = mpsc::unbounded_channel();
        let guard = ExercisePauseGuard {
            results_tx,
            rules: Some(vec![RuleId("office".into())]),
            operation_id: 1,
            generation,
        };
        drop(guard);
        assert!(matches!(
            results_rx.try_recv(),
            Ok(InternalResult::ExerciseResume { .. })
        ));
        let sink = Arc::new(crate::fakes::ExerciseSink::new());
        let _ = sink.wake_once().await;
        assert!(matches!(sink.log().last(), Some(SinkCmd::Wake)));
    }

    #[tokio::test]
    async fn emergency_wake_keeps_panicked_display_in_failed_report() {
        struct PanicWakeSink;

        #[async_trait::async_trait]
        impl CommandSink for PanicWakeSink {
            async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
                Ok(())
            }

            async fn wake(&self) -> Result<(), CmdFailure> {
                panic!("scripted daemon wake panic");
            }

            async fn wake_once(&self) -> Result<(), CmdFailure> {
                panic!("scripted daemon wake panic");
            }

            fn controller_health(&self) -> Vec<ControllerHealth> {
                vec![]
            }
        }

        let display = DisplayId("daemon-panel".into());
        let sink: Arc<dyn CommandSink> = Arc::new(PanicWakeSink);
        let mut engine = manual_display_engine(
            display.clone(),
            HashMap::from([(display.clone(), sink)]),
            Arc::new(crate::ownership::AlwaysOwned),
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        engine.handle_control(ControlMsg::EmergencyWake { reply: reply_tx });

        let report = tokio::time::timeout(Duration::from_secs(1), reply_rx)
            .await
            .expect("daemon emergency-wake reply arrives")
            .expect("daemon emergency-wake sender remains live");
        assert_eq!(report.displays.len(), 1);
        let result = &report.displays[0];
        assert_eq!(result.display, display);
        assert!(!result.ok);
        assert!(result.error.as_deref().unwrap().contains("panicked"));
    }

    #[test]
    fn emergency_report_keeps_accepted_generation() {
        let operation = AcceptedOperation {
            id: 1,
            generation: GenerationId(3),
            kind: OperationKind::EmergencyWake,
        };
        assert_eq!(operation.generation, GenerationId(3));
    }

    #[test]
    fn operation_accepted_by_old_generation_never_mutates_new() {
        assert!(!operation_completion_matches_generation(
            GenerationId(4),
            GenerationId(5)
        ));
        assert!(operation_completion_matches_generation(
            GenerationId(5),
            GenerationId(5)
        ));
    }

    // ── InhibitorKind::from_config literal pin ──────────────────────────────

    /// Each `INHIBITOR_*` const must round-trip through `from_config` to the
    /// matching [`InhibitorKind`] variant, and an unrecognized literal must
    /// yield `None`. Pins the config-string ⇄ enum mapping so a typo'd
    /// literal fails loudly here instead of silently at config-validation
    /// time (Task 2's `VALID_INHIBITORS` is a separate, config-layer
    /// concern — not referenced by this test).
    #[test]
    fn inhibitor_kind_from_config_round_trips_known_literals() {
        assert_eq!(
            InhibitorKind::from_config(INHIBITOR_USER_ACTIVITY),
            Some(InhibitorKind::UserActivity)
        );
        assert_eq!(
            InhibitorKind::from_config(INHIBITOR_AUDIO_PLAYBACK),
            Some(InhibitorKind::AudioPlayback)
        );
        assert_eq!(
            InhibitorKind::from_config(INHIBITOR_CALL),
            Some(InhibitorKind::Call)
        );
        assert_eq!(InhibitorKind::from_config("not-a-real-inhibitor"), None);
    }

    // ── Verdict logic (pure functions) ─────────────────────────────────────

    /// The decisive test for the blank step: a controller that returns Ok
    /// but whose `read_state()` reports the SAME panel state before and
    /// after the blank must surface `Failed`.  RED-first proof: if the
    /// verdict were computed from `returned_ok` only (instead of comparing
    /// `state_before` vs `state_after`), this test would mark `Confirmed`
    /// incorrectly.  The shape below — `Ok(())` + identical state before/
    /// after — is the exact failure mode the feature exists to catch.
    #[test]
    fn blank_verdict_marks_panel_unchanged_as_failed() {
        let same = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let verdict = blank_verdict(&Ok(()), same.as_ref(), same.as_ref());
        assert_eq!(
            verdict,
            ExerciseVerdict::Failed,
            "blank step must mark a no-move panel as Failed even when the command returned Ok"
        );
    }

    /// Sibling to the test above: when state DOES move, the blank step is
    /// `Confirmed`.  RED-first: a version that ignored the state comparison
    /// would also mark this Confirmed (correct here, but can't distinguish
    /// from the failure case) — the previous test is the discriminator.
    #[test]
    fn blank_verdict_marks_state_change_as_confirmed() {
        let before = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let after = Some(PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        });
        let verdict = blank_verdict(&Ok(()), before.as_ref(), after.as_ref());
        assert_eq!(verdict, ExerciseVerdict::Confirmed);
    }

    /// If the controller reports `read_state() = None` for either baseline
    /// or post-blank, the verdict is `Unconfirmable` even when the command
    /// returned Ok.  Honest, not a fake pass.
    #[test]
    fn blank_verdict_unconfirmable_when_read_state_none() {
        let none: Option<PanelState> = None;
        assert_eq!(
            blank_verdict(&Ok(()), None, Some(PanelState::default()).as_ref()),
            ExerciseVerdict::Unconfirmable,
        );
        assert_eq!(
            blank_verdict(&Ok(()), Some(PanelState::default()).as_ref(), none.as_ref()),
            ExerciseVerdict::Unconfirmable,
        );
    }

    /// Blank command itself errored → `Failed` (the command failed AND we
    /// can't observe the panel; the failure mode is the command, not the
    /// observability).
    #[test]
    fn blank_verdict_failed_when_command_errored() {
        let verdict = blank_verdict(
            &Err(CmdFailure {
                controller: "fake".into(),
                error: "E_DISPLAY_IO: scripted".into(),
            }),
            Some(PanelState::default()).as_ref(),
            Some(PanelState::default()).as_ref(),
        );
        assert_eq!(verdict, ExerciseVerdict::Failed);
    }

    /// Sibling for the wake step: state changed (post-blank → baseline)
    /// AND equals baseline → `Confirmed`.
    #[test]
    fn restore_verdict_marks_state_returned_as_confirmed() {
        let baseline = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let post_blank = Some(PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        });
        let post_wake = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let verdict = restore_verdict(
            &Ok(()),
            baseline.as_ref(),
            post_blank.as_ref(),
            post_wake.as_ref(),
        );
        assert_eq!(verdict, ExerciseVerdict::Confirmed);
    }

    /// Wake step: state changed but did NOT return to baseline → `Failed`.
    #[test]
    fn restore_verdict_marks_panel_not_returned_as_failed() {
        let baseline = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let post_blank = Some(PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(20),
        });
        let post_wake = Some(PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(40),
        });
        let verdict = restore_verdict(
            &Ok(()),
            baseline.as_ref(),
            post_blank.as_ref(),
            post_wake.as_ref(),
        );
        assert_eq!(verdict, ExerciseVerdict::Failed);
    }

    /// The "panel never moved" failure shape: baseline == post-blank ==
    /// post-wake.  Even though the wake step's post-wake state equals the
    /// baseline (which by itself would be Confirmed), the wake itself
    /// did nothing — the verdict is `Failed` because the wake should
    /// have moved the panel from its post-blank state.
    #[test]
    fn restore_verdict_marks_panel_never_moved_as_failed() {
        let frozen = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let verdict = restore_verdict(&Ok(()), frozen.as_ref(), frozen.as_ref(), frozen.as_ref());
        assert_eq!(
            verdict,
            ExerciseVerdict::Failed,
            "wake step must catch the panel-never-moved failure"
        );
    }

    // ── run_exercise_sequence end-to-end (off run loop) ────────────────────

    /// The full sequence end-to-end with a scripted read-state script:
    /// baseline → blank → wake → restore.  Each verdict is asserted
    /// individually so a regression in any step is loud.
    #[tokio::test]
    async fn exercise_sequence_confirmed_when_state_moves_and_returns() {
        use crate::fakes::{ExerciseSink, SinkCmd};

        let sink = Arc::new(ExerciseSink::new());
        // baseline = On/80, post-blank = Standby/0, post-wake = On/80.
        // Plus one extra read for the defensive restore step (returns to
        // baseline again, no-op for the verdict).
        let on_80 = PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        };
        let standby_0 = PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        };
        // read-state script: baseline, post-blank, post-wake, restore-read
        sink.push_read_state(Some(on_80.clone()));
        sink.push_read_state(Some(standby_0.clone()));
        sink.push_read_state(Some(on_80.clone()));
        sink.push_read_state(Some(on_80.clone()));

        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let report = run_exercise_sequence(
            &sink_dyn,
            Some(BlankMode::PowerOff),
            "active".to_string(),
            Vec::new(),
            DisplayId("mon".into()),
            Duration::ZERO,
        )
        .await;

        assert_eq!(
            report.steps.len(),
            4,
            "expected read/blank/wake/restore steps"
        );
        assert_eq!(
            report.steps[0].command, "read",
            "first step is the baseline read"
        );
        assert_eq!(
            report.steps[1].verdict,
            ExerciseVerdict::Confirmed,
            "blank step: state moved On→Standby"
        );
        assert_eq!(
            report.steps[2].verdict,
            ExerciseVerdict::Confirmed,
            "wake step: state returned to On"
        );
        // Restore step: pre-exercise was "active" (defensive wake path).
        assert_eq!(report.steps[3].command, "restore");

        // The sink log should show: blank, wake, wake (the defensive
        // restore wake). Three calls total — one of each kind in
        // production order: blank → wake (the test wake) → wake_once
        // (the defensive restore).
        let log = sink.log();
        assert_eq!(
            log,
            vec![
                SinkCmd::Blank(BlankMode::PowerOff),
                SinkCmd::Wake,
                SinkCmd::Wake,
            ],
        );
    }

    /// Post-wake settle retry (#34): the first post-wake read is absent,
    /// so the exercise must sleep `doctor_wake_settle` and perform
    /// exactly one bounded retry read before classifying the wake step.
    /// Paused Tokio time proves the retry genuinely waits for the
    /// configured settle duration — not a scheduling accident — by
    /// asserting the spawned sequence is still unfinished right before
    /// the deadline and only completes once the clock crosses it.
    #[tokio::test(start_paused = true)]
    async fn exercise_sequence_confirms_wake_after_settle_retry() {
        use crate::fakes::ExerciseSink;

        let sink = Arc::new(ExerciseSink::new());
        let on = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let standby = Some(PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        });

        // Script: pre-read On, blank-read Off, first post-wake read
        // None, retry read On, restore read On.
        sink.push_read_state(on.clone()); // baseline: On
        sink.push_read_state(standby); // post-blank: Off
        sink.push_read_state(None); // first post-wake read: absent
        sink.push_read_state(on.clone()); // retry read: On
        sink.push_read_state(on); // restore read: On

        let settle = Duration::from_secs(3);
        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let handle = tokio::spawn(async move {
            run_exercise_sequence(
                &sink_dyn,
                Some(BlankMode::PowerOff),
                "active".to_string(),
                Vec::new(),
                DisplayId("mon".into()),
                settle,
            )
            .await
        });

        // Let the spawned task run its synchronous prefix (baseline
        // read, blank, first post-wake read) up to the point where it
        // blocks on the settle sleep.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "exercise task must be blocked on the settle sleep before the \
             configured duration elapses, not finished early"
        );

        // Advancing by less than the full settle duration must not be
        // enough to unblock the retry.
        tokio::time::advance(settle.checked_sub(Duration::from_millis(1)).unwrap()).await;
        assert!(
            !handle.is_finished(),
            "exercise task must still be waiting just before the settle \
             deadline"
        );

        // Crossing the configured settle duration fires the sleep and
        // lets the retry read run to completion.
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            handle.is_finished(),
            "exercise task must complete once the settle duration elapses"
        );

        let report = handle.await.expect("exercise task panicked");

        assert_eq!(report.steps[2].command, "wake");
        assert_eq!(
            report.steps[2].verdict,
            ExerciseVerdict::Confirmed,
            "wake verdict must be Confirmed only after the settle retry \
             observes On"
        );
    }

    /// Decisive test: a controller that returns Ok but whose panel state
    /// does NOT change across the blank → wake sequence.  The blank
    /// step's verdict is `Failed` and the restore step is also `Failed`
    /// (the wake did not move the panel back either).  RED-first: if the
    /// verdict were computed from `returned_ok` alone, both would be
    /// `Confirmed` and the test would fail — this pins the real behavior.
    #[tokio::test]
    async fn exercise_sequence_marks_panel_unchanged_as_failed() {
        use crate::fakes::ExerciseSink;

        let sink = Arc::new(ExerciseSink::new());
        // All reads return the SAME panel state — the panel never moves.
        let frozen = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        for _ in 0..6 {
            sink.push_read_state(frozen.clone());
        }

        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let report = run_exercise_sequence(
            &sink_dyn,
            Some(BlankMode::PowerOff),
            "active".to_string(),
            Vec::new(),
            DisplayId("mon".into()),
            Duration::ZERO,
        )
        .await;

        assert_eq!(report.steps[1].command, "blank");
        assert_eq!(
            report.steps[1].verdict,
            ExerciseVerdict::Failed,
            "blank step: state did not change despite Ok return"
        );
        assert_eq!(report.steps[2].command, "wake");
        assert_eq!(
            report.steps[2].verdict,
            ExerciseVerdict::Failed,
            "wake step: state did not return to baseline"
        );
    }

    /// Unconfirmable path: a sink that returns None from `read_state()`.
    /// Every read is missing; commands still run; verdicts are
    /// `Unconfirmable` (not `Failed` — we have no readback, so we can't
    /// confirm OR fail the panel).
    #[tokio::test]
    async fn exercise_sequence_marks_no_readback_as_unconfirmable() {
        use crate::fakes::ExerciseSink;

        let sink = Arc::new(ExerciseSink::new());
        // No read_state pushed → all reads return None.

        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let report = run_exercise_sequence(
            &sink_dyn,
            Some(BlankMode::PowerOff),
            "active".to_string(),
            Vec::new(),
            DisplayId("mon".into()),
            Duration::ZERO,
        )
        .await;

        assert_eq!(report.steps[1].verdict, ExerciseVerdict::Unconfirmable);
        assert_eq!(report.steps[2].verdict, ExerciseVerdict::Unconfirmable);
        // The restore step is also Unconfirmable (no observation).
        assert_eq!(report.steps[3].verdict, ExerciseVerdict::Unconfirmable);
        // But every command still ran (the sink log shows them).
        assert!(!sink.log().is_empty());
    }

    /// Fail-safe / wake-path-sacred: an error MID-exercise still ends with
    /// a wake.  This is the cardinal rule — an exercise that catches an
    /// internal error must not leave the panel dark.  We assert on
    /// `wakes_issued` AND on the last log entry being a Wake.
    #[tokio::test]
    async fn exercise_sequence_always_wakes_even_when_blank_errors() {
        use crate::fakes::{ExerciseSink, SinkCmd};

        let sink = Arc::new(ExerciseSink::new());
        // Script: blank command itself errors, then wake succeeds, then
        // read_state returns None.  The exercise MUST still issue the
        // wake step + the defensive restore wake.
        sink.push_blank_result(Err(CmdFailure {
            controller: "fake".into(),
            error: "E_DISPLAY_IO: scripted".into(),
        }));
        // Wake results: empty queue → Ok(()).

        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let report = run_exercise_sequence(
            &sink_dyn,
            Some(BlankMode::PowerOff),
            "active".to_string(),
            Vec::new(),
            DisplayId("mon".into()),
            Duration::ZERO,
        )
        .await;

        // Blank step verdict is Failed (the command errored).
        assert_eq!(report.steps[1].verdict, ExerciseVerdict::Failed);
        assert!(!report.steps[1].returned_ok);

        // The wake + defensive wake both ran — the wake-path is sacred.
        let log = sink.log();
        assert_eq!(log.len(), 3, "expected blank + wake + restore wake");
        assert!(matches!(log[0], SinkCmd::Blank(_)));
        assert!(matches!(log[1], SinkCmd::Wake));
        assert!(matches!(log[2], SinkCmd::Wake), "restore step must wake");

        // And `wakes_issued` reports at least one wake (the test asserts
        // wake count, not log order — both wake calls happened).
        assert!(sink.wakes_issued() >= 1);
    }

    /// Manual-only display path: when the pre-exercise phase is `active`
    /// the restore step is a defensive wake (no re-blank).  When the
    /// pre-exercise phase is `blanked`, the restore step re-issues the
    /// blank.  Pin both with scripted reads so the choice is visible in
    /// the log.
    #[tokio::test]
    async fn exercise_sequence_restore_reblank_for_blanked_pre_phase() {
        use crate::fakes::{ExerciseSink, SinkCmd};

        let sink = Arc::new(ExerciseSink::new());
        // Five reads: baseline, post-blank, post-wake, restore-read,
        // restore-read.
        let on = Some(PanelState {
            power: Some(PowerState::On),
            brightness: Some(80),
        });
        let standby = Some(PanelState {
            power: Some(PowerState::Standby),
            brightness: Some(0),
        });
        sink.push_read_state(on.clone());
        sink.push_read_state(standby.clone());
        sink.push_read_state(on.clone());
        sink.push_read_state(standby.clone());

        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let report = run_exercise_sequence(
            &sink_dyn,
            Some(BlankMode::PowerOff),
            "blanked".to_string(), // restore must re-blank
            Vec::new(),
            DisplayId("mon".into()),
            Duration::ZERO,
        )
        .await;

        assert_eq!(report.steps[3].command, "restore");
        assert_eq!(
            report.steps[3].blank_mode,
            Some(BlankMode::PowerOff),
            "restore for blanked pre-phase must re-issue the blank"
        );

        // Log: blank (the exercise), wake (the test), blank (the restore).
        let log = sink.log();
        assert_eq!(log.len(), 3);
        assert!(matches!(log[0], SinkCmd::Blank(BlankMode::PowerOff)));
        assert!(matches!(log[1], SinkCmd::Wake));
        assert!(
            matches!(log[2], SinkCmd::Blank(BlankMode::PowerOff)),
            "restore for blanked pre-phase must issue a blank command, got {:?}",
            log[2]
        );
    }

    /// `paused_rules` threaded through the report so callers can
    /// surface which rules were paused (engine-side resume via
    /// `ExerciseResume` guarantees the release regardless).
    #[tokio::test]
    async fn exercise_sequence_threads_paused_rules_into_report() {
        use crate::fakes::ExerciseSink;

        let sink = Arc::new(ExerciseSink::new());
        let rules = vec![RuleId("office".into()), RuleId("lounge".into())];

        let sink_dyn: Arc<dyn CommandSink> = sink.clone();
        let report = run_exercise_sequence(
            &sink_dyn,
            Some(BlankMode::PowerOff),
            "active".to_string(),
            rules.clone(),
            DisplayId("mon".into()),
            Duration::ZERO,
        )
        .await;

        assert_eq!(report.paused_rules, rules);
    }

    #[test]
    fn display_snapshot_deserializes_legacy_without_controllers() {
        // Old daemon JSON has no "controllers" key; new binary must
        // default it to empty (serde back-compat).
        let legacy = r#"{"phase":"active","inhibited":false,"paused":false,"cmd_gen":0}"#;
        let snap: DisplaySnapshot = serde_json::from_str(legacy).unwrap();
        assert!(snap.controllers.is_empty());
    }

    #[test]
    fn display_snapshot_deserializes_legacy_without_stage() {
        // A DisplaySnapshot JSON without the "stage" key must parse with
        // stage=None (serde back-compat — same as doctor_report + controllers).
        let legacy =
            r#"{"phase":"active","inhibited":false,"paused":false,"cmd_gen":0,"controllers":[]}"#;
        let snap: DisplaySnapshot = serde_json::from_str(legacy).unwrap();
        assert!(snap.stage.is_none());
    }

    #[test]
    fn display_snapshot_serialize_omits_stage_when_none() {
        // When stage is None the key must be absent from the wire (byte-back-compat
        // with pre-stage readers — same skip_serializing_if pattern as doctor_report).
        let snap = DisplaySnapshot {
            phase: "active".into(),
            inhibited: false,
            paused: false,
            cmd_gen: 0,
            scope: DisplayScope::Private,
            owned: true,
            observed_input_code: None,
            panel_state: None,
            controllers: vec![],
            wake_attempts: 0,
            last_blank_failed: false,
            stage: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("stage"));
    }

    #[test]
    fn display_snapshot_staged_roundtrips() {
        // A Staged snapshot round-trips with a pinned wire shape.
        let snap = DisplaySnapshot {
            phase: "staged".into(),
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            scope: DisplayScope::Private,
            owned: true,
            observed_input_code: None,
            panel_state: None,
            controllers: vec![],
            wake_attempts: 0,
            last_blank_failed: false,
            stage: Some(StageInfo {
                idx: 1,
                kind: StageKind::RenderBlack,
            }),
        };
        let json = serde_json::to_string(&snap).unwrap();
        // Wire shape: idx=1, kind="render_black"
        assert!(json.contains(r#""idx":1"#));
        assert!(json.contains(r#""kind":"render_black""#));
        let back: DisplaySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase, "staged");
        let si = back.stage.unwrap();
        assert_eq!(si.idx, 1);
        assert_eq!(si.kind, StageKind::RenderBlack);
    }

    #[test]
    fn controller_role_first_is_primary_rest_fallback() {
        let h = [
            ControllerHealth {
                name: "ddcci".into(),
                role: ControllerRole::Primary,
                healthy: true,
                detail: None,
            },
            ControllerHealth {
                name: "kwin-dpms".into(),
                role: ControllerRole::Fallback,
                healthy: true,
                detail: None,
            },
        ];
        assert_eq!(h[0].role, ControllerRole::Primary);
        assert_eq!(h[1].role, ControllerRole::Fallback);
    }

    #[test]
    fn manual_defaults_returns_default_consts() {
        let holdoff = Duration::from_secs(30);
        let t = DisplayRuntimeCfg::manual_defaults(holdoff);
        assert_eq!(t.grace_period, crate::config::defaults::GRACE_PERIOD);
        assert_eq!(t.min_blank_time, crate::config::defaults::MIN_BLANK_TIME);
        assert_eq!(t.min_wake_time, crate::config::defaults::MIN_WAKE_TIME);
        assert_eq!(t.startup_holdoff, holdoff);
        assert_eq!(
            t.wake_retry_interval,
            crate::config::defaults::WAKE_RETRY_INTERVAL
        );
    }

    // ── DaemonEvent wire tolerance / new variants ────────────────────────────

    /// An event tag this build does not recognize must deserialize to
    /// `Unknown` instead of failing the whole stream — the forward-compat
    /// contract a rolling daemon-upgrade depends on (an older CLI/WebUI must
    /// not choke on a newer daemon's events).
    #[test]
    fn unknown_event_tag_parses_to_unknown() {
        let e: DaemonEvent = serde_json::from_str(r#"{"event":"from_the_future","x":1}"#).unwrap();
        assert!(matches!(e, DaemonEvent::Unknown));
    }

    /// `WearSnapshot` round-trips through the wire with the expected tag and
    /// fields.
    #[test]
    fn wear_snapshot_round_trips() {
        // DisplayId is a tuple newtype with no `From<&str>` — construct it
        // directly (repo convention).
        let e = DaemonEvent::WearSnapshot {
            display: DisplayId("m".into()),
            total_on_hours: 1.5,
            sample_count: 3,
            wear_attribution_mode: crate::wear::WearAttributionMode::Uniform,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"event\":\"wear_snapshot\""));
        assert!(matches!(
            serde_json::from_str(&s).unwrap(),
            DaemonEvent::WearSnapshot { .. }
        ));
    }

    /// `OperationsChanged` (issue #184) round-trips through the wire with
    /// the expected tag and fields, and the `Unknown` catch-all still
    /// traps unrecognized tags so older clients keep working.
    #[test]
    fn operations_changed_event_round_trips() {
        let ev = DaemonEvent::OperationsChanged {
            exercise_in_flight: vec!["studio".to_string(), "main".to_string()],
            emergency_wake_in_flight: true,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "event": "operations_changed",
                "exercise_in_flight": ["studio", "main"],
                "emergency_wake_in_flight": true,
            })
        );
        let parsed: DaemonEvent = serde_json::from_value(json).unwrap();
        match parsed {
            DaemonEvent::OperationsChanged {
                exercise_in_flight,
                emergency_wake_in_flight,
            } => {
                assert_eq!(exercise_in_flight, vec!["studio", "main"]);
                assert!(emergency_wake_in_flight);
            }
            other => panic!("expected OperationsChanged verbatim, got {other:?}"),
        }
        // The catch-all still routes unknown tags to Unknown — this is the
        // forward-compat path a rolling daemon-upgrade relies on.
        let unknown: DaemonEvent =
            serde_json::from_str(r#"{"event":"from_the_future","x":1}"#).unwrap();
        assert!(matches!(unknown, DaemonEvent::Unknown));
    }

    #[test]
    fn wear_snapshot_preserves_additive_attribution_mode() {
        let wire = r#"{"event":"wear_snapshot","display":"m","total_on_hours":1.5,"sample_count":3,"wear_attribution_mode":"sampled"}"#;
        let event: DaemonEvent = serde_json::from_str(wire).unwrap();

        let round_trip = serde_json::to_string(&event).unwrap();
        assert!(round_trip.contains("\"wear_attribution_mode\":\"sampled\""));
    }

    /// `CompensationAdvisory` round-trips through the wire with the expected
    /// tag and fields.
    #[test]
    fn compensation_advisory_round_trips() {
        let e = DaemonEvent::CompensationAdvisory {
            display: DisplayId("m".into()),
            hours_since_long_dwell: 12,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"event\":\"compensation_advisory\""));
        assert!(matches!(
            serde_json::from_str(&s).unwrap(),
            DaemonEvent::CompensationAdvisory { .. }
        ));
    }

    /// `BlankFailure` round-trips through the wire with the expected tag.
    #[test]
    fn blank_failure_round_trips_with_tag() {
        let e = DaemonEvent::BlankFailure {
            display: DisplayId("m".into()),
            controller: "ddcci".into(),
            detail: "E_DISPLAY_IO: bus gone".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"event\":\"blank_failure\""));
        assert!(matches!(
            serde_json::from_str(&s).unwrap(),
            DaemonEvent::BlankFailure { .. }
        ));
    }

    /// First in-repo exercise of `#[serde(default)]` on fields of an
    /// internally tagged variant (spec F18) — `display` is REQUIRED, the
    /// rest default.
    #[test]
    fn new_variants_deserialize_with_missing_defaulted_fields() {
        let e: DaemonEvent =
            serde_json::from_str(r#"{"event":"blank_failure","display":"m"}"#).unwrap();
        match e {
            DaemonEvent::BlankFailure {
                controller, detail, ..
            } => {
                assert_eq!(controller, "");
                assert_eq!(detail, "");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let e: DaemonEvent =
            serde_json::from_str(r#"{"event":"wake_recovered","display":"m"}"#).unwrap();
        assert!(matches!(e, DaemonEvent::WakeRecovered { attempts: 0, .. }));
        // display truly required:
        assert!(serde_json::from_str::<DaemonEvent>(r#"{"event":"blank_recovered"}"#).is_err());
    }

    /// Old `DisplaySnapshot` JSON without the two new failure-tracking keys
    /// must still parse, defaulting them to the "healthy" values.
    #[test]
    fn legacy_display_snapshot_parses_without_new_keys() {
        let json =
            r#"{"phase":"active","inhibited":false,"paused":false,"cmd_gen":3,"controllers":[]}"#;
        let d: DisplaySnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(d.wake_attempts, 0);
        assert!(!d.last_blank_failed);
    }

    #[test]
    fn legacy_display_snapshot_defaults_private_owned_and_unknown_panel() {
        let json =
            r#"{"phase":"active","inhibited":false,"paused":false,"cmd_gen":3,"controllers":[]}"#;
        let snapshot: DisplaySnapshot = serde_json::from_str(json).unwrap();

        assert_eq!(snapshot.scope, DisplayScope::Private);
        assert!(snapshot.owned);
        assert_eq!(snapshot.observed_input_code, None);
        assert_eq!(snapshot.panel_state, None);
    }

    #[test]
    fn shared_display_snapshot_uses_hardware_observation_not_local_phase() {
        let display = DisplayId("shared".into());
        let coordination = crate::coordination::CoordinationHandle::new([display.clone()]);
        coordination.record_success(
            &display,
            0x10,
            0x0f,
            Some(PanelState {
                power: Some(PowerState::Standby),
                brightness: Some(0),
            }),
        );
        let mut engine = RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![DisplayRuntimeCfg {
                    display: display.clone(),
                    blank_mode: BlankMode::PowerOff,
                    ladder: vec![LadderStage {
                        kind: StageKind::Controller(BlankMode::PowerOff),
                        dwell: None,
                    }],
                    timings: DisplayRuntimeCfg::manual_defaults(Duration::ZERO),
                }],
                sensors: vec![],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid"),
            HashMap::new(),
            HashMap::new(),
            Arc::new(crate::coordination::CoordinationGate::new(
                coordination.clone(),
            )),
        )
        .expect("shared display engine config is valid")
        .with_coordination_handle(coordination);

        let snapshot = snapshot_of(&mut engine);
        let display = &snapshot.displays[0].1;
        assert_eq!(display.phase, "active");
        assert_eq!(display.scope, DisplayScope::Shared);
        assert!(!display.owned);
        assert_eq!(display.observed_input_code, Some(0x10));
        assert_eq!(
            display.panel_state.as_ref().and_then(|state| state.power),
            Some(PowerState::Standby)
        );
    }

    // ── `reported` cold-start diagnostic (spec §5) ──────────────────────────

    /// A minimal engine with exactly ONE sensor and no rules/displays — for
    /// the `reported` tests below, which drive `handle_presence_event` and
    /// `handle_control(Snapshot)` directly (private-access, co-located).
    fn engine_with_sensor(id: &str) -> RulesEngine {
        let sid = SensorId(id.into());
        RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![],
                sensors: vec![SensorRuntimeCfg {
                    sensor: sid.clone(),
                    kind: SensorKind::Presence,
                    hold_time: None,
                    stale_timeout: Duration::from_secs(3600),
                }],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[sid]).expect("single-sensor empty-zone engine is valid"),
            HashMap::new(),
            HashMap::new(),
            Arc::new(crate::ownership::AlwaysOwned),
        )
        .expect("single-sensor engine config is valid")
    }

    /// Take a synchronous snapshot from an engine via `handle_control` —
    /// `send_snapshot` replies inline (no `run()` loop needed).
    fn snapshot_of(engine: &mut RulesEngine) -> StateSnapshot {
        let (tx, mut rx) = oneshot::channel();
        engine.handle_control(ControlMsg::Snapshot(tx));
        rx.try_recv().expect("snapshot reply sent inline")
    }

    /// F10: a foreign ownership-loss feed is suppressed when
    /// the claim runtime has armed a suppression deadline. The
    /// rules engine does NOT feed
    /// `Input::OwnershipChanged(false)` while the deadline is
    /// in the future. Once the deadline elapses, the next
    /// ownership-poll feeds normally.
    #[tokio::test]
    async fn f10_claim_suppression_gates_ownership_loss_feed() {
        use crate::ownership::OwnershipGate;
        // A test ownership gate that holds a single verdict;
        // we drive the suppression round-trip without flipping
        // the gate (the rules engine consults the suppression
        // gate BEFORE the ownership gate for the loss path,
        // so the state machine never moves when suppression
        // is armed).
        struct HeldGate {
            owned: std::sync::Mutex<bool>,
        }
        impl OwnershipGate for HeldGate {
            fn owns(&self, _display: &DisplayId) -> bool {
                *self.owned.lock().unwrap()
            }
        }
        let gate: Arc<dyn OwnershipGate> = Arc::new(HeldGate {
            owned: std::sync::Mutex::new(true),
        });
        let cfg = RulesEngineConfig {
            rules: vec![],
            displays: vec![DisplayRuntimeCfg {
                display: DisplayId("mon".into()),
                blank_mode: crate::types::BlankMode::PowerOff,
                timings: DisplayRuntimeCfg::manual_defaults(Duration::ZERO),
                ladder: vec![],
            }],
            sensors: vec![],
            doctor_wake_settle: Duration::from_secs(3),
        };
        let mut engine = RulesEngine::new(
            cfg,
            ZoneEngine::new(vec![], &[]).expect("empty zones"),
            HashMap::new(),
            HashMap::new(),
            gate.clone(),
        )
        .expect("valid config");
        let display = DisplayId("mon".into());
        // Arm a future suppression deadline (F10 active).
        let future = Instant::now() + Duration::from_secs(60);
        engine.handle_control(ControlMsg::SetClaimSuppression {
            display: display.clone(),
            until: Some(future),
        });
        // Drive a foreign ownership-loss feed — the loss is
        // NOT fed (suppression lifts it). The state
        // machine's phase for `mon` remains the seed value.
        engine.handle_control(ControlMsg::OwnershipPoll {
            display: display.clone(),
        });
        let snap = snapshot_of(&mut engine);
        let d = snap
            .displays
            .iter()
            .find(|(id, _)| id == "mon")
            .expect("mon in snapshot");
        assert_eq!(
            d.1.phase, "active",
            "ownership-loss feed is suppressed by the claim deadline"
        );
        assert!(d.1.owned, "ownership gate still says owned=true (pre-flip)");

        // Lift the suppression and re-feed — still owned, no
        // transition expected (the gate hasn't flipped).
        engine.handle_control(ControlMsg::SetClaimSuppression {
            display: display.clone(),
            until: None,
        });
        engine.handle_control(ControlMsg::OwnershipPoll {
            display: display.clone(),
        });
        let snap = snapshot_of(&mut engine);
        let d = snap
            .displays
            .iter()
            .find(|(id, _)| id == "mon")
            .expect("mon in snapshot");
        assert_eq!(d.1.phase, "active");
    }

    /// `KvmStatus`: `SetKvmStatus` replaces the snapshot fold.
    #[tokio::test]
    async fn kvm_status_set_replaces_snapshot_fold() {
        use crate::config::KeymapConfig;
        let mut engine = minimal_engine();
        let status = crate::rules::KvmStatus {
            keymap: KeymapConfig {
                claim_hotkey: Some("Meta+F12".to_owned()),
            },
            switch_capable_displays: vec![DisplayId("mon".into())],
            activity_following: true,
            push_capable_displays: vec![],
        };
        engine.handle_control(ControlMsg::SetKvmStatus(status));
        let snap = snapshot_of(&mut engine);
        let kvm = snap
            .kvm
            .expect("KvmStatus populated by SetKvmStatus control message");
        assert_eq!(kvm.switch_capable_displays, vec![DisplayId("mon".into())]);
        assert!(kvm.activity_following);
        assert_eq!(kvm.keymap.claim_hotkey.as_deref(), Some("Meta+F12"));
    }

    #[tokio::test]
    async fn generation_barrier_drains_before_ack() {
        let (events_tx, events_rx) = mpsc::channel(4);
        let (ctl_tx, ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let engine = engine_with_sensor("desk");
        let task = tokio::spawn(engine.run(events_rx, ctl_rx, cancel.clone()));

        events_tx
            .send(PresenceEvent::new(
                SensorId("desk".into()),
                SensorState::Present,
                Timestamp::now(),
            ))
            .await
            .unwrap();
        ctl_tx
            .send(ControlMsg::SetPendingReload(Some("queued".into())))
            .await
            .unwrap();
        let (barrier_tx, barrier_rx) = oneshot::channel();
        ctl_tx
            .send(ControlMsg::GenerationBarrier(barrier_tx))
            .await
            .unwrap();
        barrier_rx.await.expect("barrier acknowledgement");

        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        ctl_tx
            .send(ControlMsg::Snapshot(snapshot_tx))
            .await
            .unwrap();
        let snapshot = tokio::time::timeout(Duration::from_secs(1), snapshot_rx)
            .await
            .expect("snapshot reply arrives")
            .expect("engine remains live");
        assert!(snapshot.sensors.iter().any(|sensor| sensor.reported));
        assert_eq!(snapshot.pending_reload.as_deref(), Some("queued"));

        cancel.cancel();
        task.await.unwrap();
    }

    #[test]
    fn reported_false_until_first_event_then_true() {
        let mut engine = engine_with_sensor("desk");

        let before = snapshot_of(&mut engine);
        let sensor = before
            .sensors
            .iter()
            .find(|s| s.id == "desk")
            .expect("sensor 'desk' in snapshot");
        assert!(
            !sensor.reported,
            "a sensor that has never delivered an event must show reported == false"
        );

        engine.handle_presence_event(PresenceEvent::new(
            SensorId("desk".into()),
            SensorState::Present,
            Timestamp::now(),
        ));

        let after = snapshot_of(&mut engine);
        let sensor = after
            .sensors
            .iter()
            .find(|s| s.id == "desk")
            .expect("sensor 'desk' in snapshot");
        assert!(
            sensor.reported,
            "reported must flip to true after the first PresenceEvent"
        );

        // A further state flip (Present → Absent) must not clear it — the
        // set records "has reported since start", not "is currently
        // present".
        engine.handle_presence_event(PresenceEvent::new(
            SensorId("desk".into()),
            SensorState::Absent,
            Timestamp::now(),
        ));
        let still_after = snapshot_of(&mut engine);
        let sensor = still_after
            .sensors
            .iter()
            .find(|s| s.id == "desk")
            .expect("sensor 'desk' in snapshot");
        assert!(
            sensor.reported,
            "reported must stay true across subsequent state flips"
        );
    }

    /// A minimal engine with exactly ONE `Motion` sensor configured with a
    /// hold time — for the hold-configured-sensor `reported` test below.
    fn engine_with_hold_sensor(id: &str, hold: Duration) -> RulesEngine {
        let sid = SensorId(id.into());
        RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![],
                sensors: vec![SensorRuntimeCfg {
                    sensor: sid.clone(),
                    kind: SensorKind::Motion,
                    hold_time: Some(hold),
                    stale_timeout: Duration::from_secs(3600),
                }],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[sid]).expect("single-sensor empty-zone engine is valid"),
            HashMap::new(),
            HashMap::new(),
            Arc::new(crate::ownership::AlwaysOwned),
        )
        .expect("single hold-sensor engine config is valid")
    }

    /// T3 review S1: no test in the original diff configured a `hold_time`
    /// sensor and drove `reported` through it — the "has reported since
    /// start" guarantee for hold-configured sensors was true only because
    /// `HoldState` always starts disarmed (`armed_until: None`), so a
    /// sensor's very first-ever event can never be swallowed (arming only
    /// happens on a `Present` event that has already passed the filter).
    ///
    /// RED-first note: the review traced this exhaustively and concluded
    /// there is NO reachable shape where a hold-configured sensor's first
    /// event is swallowed — so this test cannot be written to fail under
    /// the old post-filter insert placement (a genuine RED-first case
    /// doesn't exist here). Per the review's own fallback, this test pins
    /// the invariant directly instead: a hold-configured sensor's first
    /// event — even `Absent`, the state the filter CAN swallow once armed
    /// — must flip `reported` true, and a later event that IS actually
    /// swallowed (once the hold is armed) must not un-set it.
    #[test]
    fn reported_flips_true_for_hold_configured_sensor() {
        let mut engine = engine_with_hold_sensor("motion1", Duration::from_secs(30));

        let before = snapshot_of(&mut engine);
        let sensor = before
            .sensors
            .iter()
            .find(|s| s.id == "motion1")
            .expect("sensor 'motion1' in snapshot");
        assert!(
            !sensor.reported,
            "hold-configured sensor must show reported == false before any event"
        );

        // First-ever event, Absent — the state `apply_hold_filter` swallows
        // once a hold is armed. `HoldState` starts disarmed, so today this
        // still passes through, but `reported` must be set from the raw
        // event regardless of the filter's verdict.
        engine.handle_presence_event(PresenceEvent::new(
            SensorId("motion1".into()),
            SensorState::Absent,
            Timestamp::now(),
        ));

        let after = snapshot_of(&mut engine);
        let sensor = after
            .sensors
            .iter()
            .find(|s| s.id == "motion1")
            .expect("sensor 'motion1' in snapshot");
        assert!(
            sensor.reported,
            "a hold-configured sensor's first-ever event must flip reported == true"
        );

        // Arm the hold with a Present event, then drive an Absent while
        // armed — this one IS swallowed by the filter (stashed as
        // `pending_absent`, replayed at hold expiry). `reported` must stay
        // true; it must not depend on this swallowed event reaching the
        // insert.
        engine.handle_presence_event(PresenceEvent::new(
            SensorId("motion1".into()),
            SensorState::Present,
            Timestamp::now(),
        ));
        engine.handle_presence_event(PresenceEvent::new(
            SensorId("motion1".into()),
            SensorState::Absent,
            Timestamp::now(),
        ));
        let still_after = snapshot_of(&mut engine);
        let sensor = still_after
            .sensors
            .iter()
            .find(|s| s.id == "motion1")
            .expect("sensor 'motion1' in snapshot");
        assert!(
            sensor.reported,
            "reported must remain true even while a later event is swallowed by an armed hold"
        );
    }

    #[test]
    fn legacy_sensor_snapshot_parses_without_reported() {
        // Old daemon JSON has no "reported" key; new binary must default
        // it to false (serde back-compat — honest: unknown provenance).
        let json = r#"{"id":"desk","state":"present","last_seen_secs_ago":2}"#;
        let s: SensorSnapshot = serde_json::from_str(json).unwrap();
        assert!(!s.reported);
    }

    #[test]
    fn seed_sensor_reported_surfaces_in_snapshot() {
        // Fresh engine, zero events processed — seed_sensor_reported is the
        // reload/restore carry-over seam; it must surface in the snapshot
        // without any PresenceEvent ever having been processed.
        let mut engine = engine_with_sensor("desk");
        engine.seed_sensor_reported(&SensorId("desk".into()));

        let snap = snapshot_of(&mut engine);
        let sensor = snap
            .sensors
            .iter()
            .find(|s| s.id == "desk")
            .expect("sensor 'desk' in snapshot");
        assert!(
            sensor.reported,
            "seed_sensor_reported must surface as reported == true with zero events processed"
        );
    }

    /// Minimal engine used by the `PublishDaemonEvent` tests below — no
    /// rules/displays/sensors, `AlwaysOwned` gate.  `handle_control` is
    /// synchronous and self-contained (no `run()` loop needed), so these
    /// tests drive it directly.
    fn minimal_engine() -> RulesEngine {
        RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![],
                sensors: vec![],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid"),
            HashMap::new(),
            HashMap::new(),
            Arc::new(crate::ownership::AlwaysOwned),
        )
        .expect("minimal engine config is valid")
    }

    /// Shared mutable ownership gate for control-path tests.
    struct FlipGate(Arc<std::sync::atomic::AtomicBool>);

    impl OwnershipGate for FlipGate {
        fn owns(&self, _display: &DisplayId) -> bool {
            self.0.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn manual_display_engine(
        display: DisplayId,
        executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
        ownership: Arc<dyn OwnershipGate>,
    ) -> RulesEngine {
        RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![DisplayRuntimeCfg {
                    display,
                    blank_mode: BlankMode::PowerOff,
                    ladder: vec![LadderStage {
                        kind: StageKind::Controller(BlankMode::PowerOff),
                        dwell: None,
                    }],
                    timings: DisplayRuntimeCfg::manual_defaults(Duration::ZERO),
                }],
                sensors: vec![],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid"),
            executors,
            HashMap::new(),
            ownership,
        )
        .expect("one-display engine config is valid")
    }

    /// Build a manual one-display engine with a caller-supplied ladder and
    /// render sinks.  Used by the soft/hard blank tests (issue #124) so
    /// they can observe the difference between "walk the render ladder" and
    /// "issue a primary controller blank."
    #[allow(clippy::implicit_hasher)]
    fn manual_engine_with_ladder(
        display: DisplayId,
        executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
        render_sinks: HashMap<DisplayId, Arc<dyn RenderSink>>,
        ladder: Vec<LadderStage>,
        ownership: Arc<dyn OwnershipGate>,
    ) -> RulesEngine {
        RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![DisplayRuntimeCfg {
                    display,
                    blank_mode: BlankMode::PowerOff,
                    ladder,
                    timings: DisplayRuntimeCfg::manual_defaults(Duration::ZERO),
                }],
                sensors: vec![],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid"),
            executors,
            render_sinks,
            ownership,
        )
        .expect("one-display ladder engine config is valid")
    }

    #[tokio::test]
    async fn ownership_poll_refeeds_changed_cached_verdict_without_timer_sweep() {
        let display = DisplayId("mon".into());
        let sink = Arc::new(RecordingSink::new());
        let mut executors: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        executors.insert(display.clone(), sink.clone());
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut engine = manual_display_engine(
            display.clone(),
            executors,
            Arc::new(FlipGate(Arc::clone(&owned))),
        );

        assert_eq!(engine.last_owned.get(&display), Some(&false));
        let before_poll = Tick::now();
        engine.step_machine(&display, Input::ZonePresent(false), before_poll);
        owned.store(true, std::sync::atomic::Ordering::Relaxed);

        engine.handle_control(ControlMsg::OwnershipPoll {
            display: display.clone(),
        });

        assert_eq!(engine.last_owned.get(&display), Some(&true));

        engine.step_machine(
            &display,
            Input::Tick,
            Tick(before_poll.0 + Duration::from_secs(3600)),
        );
        let result = engine
            .results_rx
            .recv()
            .await
            .expect("post-poll tick reports the blank command");
        assert!(matches!(result, InternalResult::Blank { .. }));
        assert!(
            sink.log()
                .iter()
                .any(|(_, command)| matches!(command, SinkCmd::Blank(BlankMode::PowerOff))),
            "the ownership gain must reach the machine before its next timer tick"
        );
    }

    #[test]
    fn ownership_poll_unchanged_verdict_is_noop() {
        use crate::state_machine::Phase;

        let display = DisplayId("mon".into());
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut engine =
            manual_display_engine(display.clone(), HashMap::new(), Arc::new(FlipGate(owned)));
        let timings = DisplayRuntimeCfg::manual_defaults(Duration::ZERO);
        let (restored, effects) = DisplayStateMachine::restore(
            timings,
            vec![LadderStage {
                kind: StageKind::Controller(BlankMode::PowerOff),
                dwell: None,
            }],
            Phase::Blanked,
            1,
            Tick::now(),
        );
        assert!(effects.is_empty());
        engine.machines.insert(display.clone(), restored);

        let (subscribe, mut events) = oneshot::channel();
        engine.handle_control(ControlMsg::SubscribeEvents(subscribe));
        let mut events = events.try_recv().expect("subscription reply sent inline");

        engine.handle_control(ControlMsg::OwnershipPoll {
            display: display.clone(),
        });

        // A duplicate false input would yield the restored Blanked machine.
        assert_eq!(*engine.machines[&display].phase(), Phase::Blanked);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn force_blank_bypasses_never_owned_gate() {
        let display = DisplayId("mon".into());
        let sink = Arc::new(RecordingSink::new());
        let mut executors: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        executors.insert(display.clone(), sink.clone());
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut engine =
            manual_display_engine(display.clone(), executors, Arc::new(FlipGate(owned)));

        engine.handle_control(ControlMsg::ForceBlank(display));
        let result = engine
            .results_rx
            .recv()
            .await
            .expect("forced blank command reports a result");

        // Regression pin: operator force commands deliberately bypass ownership.
        assert!(matches!(result, InternalResult::Blank { .. }));
        assert!(
            sink.log()
                .iter()
                .any(|(_, command)| matches!(command, SinkCmd::Blank(BlankMode::PowerOff))),
            "ForceBlank must issue a command when the ownership gate denies control"
        );
    }

    // ── Soft vs Hard blank (issue #124) ────────────────────────────────

    use crate::fakes::{RecordingRenderSink, RenderCmd};

    /// `ControlMsg::SoftBlank` must walk the configured render/stage/controller
    /// ladder from its FIRST stage — never skip past the render overlay.  This
    /// is the safety guarantee the issue-#124 fix relies on: a soft blank on a
    /// shared panel must NOT hard-power the panel before the render surface
    /// is shown.  Pin the ladder-shape distinction so a future refactor that
    /// routes Soft through `ForceBlank`'s primary-mode path fails loudly.
    #[tokio::test]
    async fn soft_blank_walks_ladder_from_first_stage() {
        let display = DisplayId("mon".into());
        let cmd_sink = Arc::new(RecordingSink::new());
        let render_sink = Arc::new(RecordingRenderSink::new());
        let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        execs.insert(display.clone(), cmd_sink.clone());
        let mut renders: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
        renders.insert(display.clone(), render_sink.clone());
        let ladder = vec![
            LadderStage {
                kind: StageKind::RenderBlack,
                dwell: Some(Duration::from_secs(30)),
            },
            LadderStage {
                kind: StageKind::Controller(BlankMode::PowerOff),
                dwell: None,
            },
        ];
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut engine = manual_engine_with_ladder(
            display.clone(),
            execs,
            renders,
            ladder,
            Arc::new(FlipGate(owned)),
        );

        engine.handle_control(ControlMsg::SoftBlank(display.clone()));
        // Drain the spawned render/show result via the engine's own
        // results channel — event-driven synchronization (no sleep, which
        // the test-timing policy would reject).
        let result = engine
            .results_rx
            .recv()
            .await
            .expect("soft blank renders the render stage and reports a result");
        assert!(
            matches!(result, crate::rules::InternalResult::Render { .. }),
            "SoftBlank must enter the ladder at stage 0 (RenderBlack), got {result:?}"
        );

        let render_log = render_sink.log();
        assert!(
            render_log.iter().any(|(_, c)| matches!(
                c,
                RenderCmd::Show {
                    kind: StageKind::RenderBlack,
                    ..
                }
            )),
            "SoftBlank must enter the ladder at stage 0 (RenderBlack), got {render_log:?}"
        );
        let cmd_log = cmd_sink.log();
        assert!(
            !cmd_log.iter().any(|(_, c)| matches!(c, SinkCmd::Blank(_))),
            "SoftBlank must NOT issue a controller blank before the render dwell elapses, got {cmd_log:?}"
        );
    }

    /// `ControlMsg::ForceBlank` (Hard) must skip the render ladder and issue
    /// the primary controller blank immediately — the operator-override path
    /// unchanged by issue #124.  This is the discriminant for the soft/hard
    /// split: a regression that re-routes `ForceBlank` through the ladder would
    /// break every existing "Force blank" surface (tray, web, `dormantctl
    /// blank --hard`).
    #[tokio::test]
    async fn hard_blank_skips_to_primary_hardware_mode() {
        let display = DisplayId("mon".into());
        let cmd_sink = Arc::new(RecordingSink::new());
        let render_sink = Arc::new(RecordingRenderSink::new());
        let mut execs: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        execs.insert(display.clone(), cmd_sink.clone());
        let mut renders: HashMap<DisplayId, Arc<dyn RenderSink>> = HashMap::new();
        renders.insert(display.clone(), render_sink.clone());
        let ladder = vec![
            LadderStage {
                kind: StageKind::RenderBlack,
                dwell: Some(Duration::from_secs(30)),
            },
            LadderStage {
                kind: StageKind::Controller(BlankMode::PowerOff),
                dwell: None,
            },
        ];
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut engine = manual_engine_with_ladder(
            display.clone(),
            execs,
            renders,
            ladder,
            Arc::new(FlipGate(owned)),
        );

        engine.handle_control(ControlMsg::ForceBlank(display.clone()));
        let result = engine
            .results_rx
            .recv()
            .await
            .expect("ForceBlank reports a blank command result");

        assert!(matches!(result, InternalResult::Blank { .. }));
        let cmd_log = cmd_sink.log();
        assert!(
            cmd_log
                .iter()
                .any(|(_, c)| matches!(c, SinkCmd::Blank(BlankMode::PowerOff))),
            "ForceBlank must issue the primary controller blank immediately, got {cmd_log:?}"
        );
        let render_log = render_sink.log();
        assert!(
            !render_log
                .iter()
                .any(|(_, c)| matches!(c, RenderCmd::Show { .. })),
            "ForceBlank must NOT enter the render ladder first, got {render_log:?}"
        );
    }

    #[tokio::test]
    async fn force_wake_bypasses_never_owned_gate() {
        let display = DisplayId("mon".into());
        let sink = Arc::new(RecordingSink::new());
        let mut executors: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
        executors.insert(display.clone(), sink.clone());
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut engine =
            manual_display_engine(display.clone(), executors, Arc::new(FlipGate(owned)));

        engine.handle_control(ControlMsg::ForceBlank(display.clone()));
        let blank_result = engine
            .results_rx
            .recv()
            .await
            .expect("setup forced blank command reports a result");
        engine.handle_internal_result(blank_result, Tick::now());

        engine.handle_control(ControlMsg::ForceWake(display));
        let result = engine
            .results_rx
            .recv()
            .await
            .expect("forced wake command reports a result");

        // Regression pin: recovery wake remains available after ownership yield.
        assert!(matches!(result, InternalResult::Wake { .. }));
        assert!(
            sink.log()
                .iter()
                .any(|(_, command)| matches!(command, SinkCmd::Wake)),
            "ForceWake must issue a command when the ownership gate denies control"
        );
    }

    #[test]
    fn state_snapshot_rollback_field_is_additive() {
        let legacy = r#"{"sensors":[],"zones":[],"displays":[],"pending_reload":null}"#;
        let parsed: StateSnapshot = serde_json::from_str(legacy).unwrap();
        assert!(parsed.rollback.is_none());

        let json = serde_json::to_value(parsed).unwrap();
        assert!(json.get("rollback").is_none());
    }

    #[test]
    fn state_snapshot_sampling_status_field_is_additive() {
        let legacy = r#"{"sensors":[],"zones":[],"displays":[],"pending_reload":null}"#;
        let parsed: StateSnapshot = serde_json::from_str(legacy).unwrap();
        let json = serde_json::to_value(parsed).unwrap();

        assert!(json.get("wear_sampling_status").is_none());
    }

    #[test]
    fn rollback_status_parks_in_snapshot_and_clears() {
        let mut engine = minimal_engine();
        let status = RollbackStatus {
            failed_fp: "12:deadbeef".to_string(),
            lkg_fp: "11:cafebabe".to_string(),
            detail: "rolled back to last-known-good".to_string(),
            recovery_command: None,
        };

        engine.set_rollback(Some(status.clone()));
        assert_eq!(snapshot_of(&mut engine).rollback, Some(status));

        engine.set_rollback(None);
        assert!(snapshot_of(&mut engine).rollback.is_none());
    }

    /// `ControlMsg::PublishDaemonEvent` is the tracker's publish path (P3):
    /// a daemon-lifetime tracker cannot hold the engine's private
    /// per-generation `event_tx`, so it publishes through this control
    /// message instead.  A subscriber attached before the publish must see
    /// the event verbatim.
    #[test]
    fn publish_daemon_event_reaches_subscriber_verbatim() {
        let mut engine = minimal_engine();

        let (sub_tx, mut sub_rx) = oneshot::channel();
        engine.handle_control(ControlMsg::SubscribeEvents(sub_tx));
        let mut sub = sub_rx.try_recv().expect("subscribe reply sent inline");

        let ev = DaemonEvent::WearSnapshot {
            display: DisplayId("m".into()),
            total_on_hours: 1.5,
            sample_count: 3,
            wear_attribution_mode: crate::wear::WearAttributionMode::Uniform,
        };
        engine.handle_control(ControlMsg::PublishDaemonEvent(ev));

        let got = sub.try_recv().expect("published event delivered");
        match got {
            DaemonEvent::WearSnapshot {
                display,
                total_on_hours,
                sample_count,
                ..
            } => {
                assert_eq!(display, DisplayId("m".into()));
                assert!((total_on_hours - 1.5).abs() < f64::EPSILON);
                assert_eq!(sample_count, 3);
            }
            other => panic!("expected WearSnapshot verbatim, got {other:?}"),
        }
    }

    fn subscribed_manual_engine(
        display: DisplayId,
    ) -> (RulesEngine, broadcast::Receiver<DaemonEvent>) {
        let mut engine = manual_display_engine(
            display,
            HashMap::new(),
            Arc::new(crate::ownership::AlwaysOwned),
        );
        let (sub_tx, mut sub_rx) = oneshot::channel();
        engine.handle_control(ControlMsg::SubscribeEvents(sub_tx));
        (
            engine,
            sub_rx.try_recv().expect("subscription reply sent inline"),
        )
    }

    #[test]
    fn pause_changed_active_display_emits_paused_true() {
        let display = DisplayId("mon".into());
        let (mut engine, mut events) = subscribed_manual_engine(display.clone());
        assert!(
            engine.machines.contains_key(&display),
            "pause target display exists"
        );

        engine.step_machine(&display, Input::Pause { until: None }, Tick::now());
        assert!(matches!(
            events.try_recv(),
            Ok(DaemonEvent::PauseChanged {
                paused: true,
                rule: None,
                ..
            })
        ));
    }

    #[test]
    fn pause_changed_explicit_resume_emits_paused_false() {
        let display = DisplayId("mon".into());
        let (mut engine, mut events) = subscribed_manual_engine(display.clone());
        assert!(
            engine.machines.contains_key(&display),
            "resume target display exists"
        );
        let now = Tick::now();

        engine.step_machine(&display, Input::Pause { until: None }, now);
        let _ = events
            .try_recv()
            .expect("pause path reached and emitted setup event");
        engine.step_machine(&display, Input::Resume, now);
        assert!(matches!(
            events.try_recv(),
            Ok(DaemonEvent::PauseChanged {
                paused: false,
                rule: None,
                ..
            })
        ));
    }

    #[test]
    fn pause_changed_auto_resume_expiry_emits_paused_false() {
        let display = DisplayId("mon".into());
        let (mut engine, mut events) = subscribed_manual_engine(display.clone());
        assert!(
            engine.machines.contains_key(&display),
            "expiry target display exists"
        );
        let now = Tick::now();
        let deadline = Tick(now.0 + Duration::from_secs(1));

        engine.step_machine(
            &display,
            Input::Pause {
                until: Some(deadline),
            },
            now,
        );
        let _ = events
            .try_recv()
            .expect("pause path reached and emitted setup event");
        engine.step_machine(&display, Input::Tick, deadline);
        assert!(matches!(
            events.try_recv(),
            Ok(DaemonEvent::PauseChanged {
                paused: false,
                rule: None,
                ..
            })
        ));
    }

    #[test]
    fn pause_changed_repeated_pause_emits_nothing() {
        let display = DisplayId("mon".into());
        let (mut engine, mut events) = subscribed_manual_engine(display.clone());
        assert!(
            engine.machines.contains_key(&display),
            "pause target display exists"
        );
        let now = Tick::now();

        engine.step_machine(&display, Input::Pause { until: None }, now);
        let _ = events
            .try_recv()
            .expect("pause path reached and emitted setup event");
        engine.step_machine(&display, Input::Pause { until: None }, now);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn pause_changed_repeated_resume_emits_nothing() {
        let display = DisplayId("mon".into());
        let (mut engine, mut events) = subscribed_manual_engine(display.clone());
        assert!(
            engine.machines.contains_key(&display),
            "resume target display exists"
        );
        let now = Tick::now();

        engine.step_machine(&display, Input::Pause { until: None }, now);
        let _ = events
            .try_recv()
            .expect("pause path reached and emitted setup event");
        engine.step_machine(&display, Input::Resume, now);
        let _ = events
            .try_recv()
            .expect("resume path reached and emitted setup event");
        engine.step_machine(&display, Input::Resume, now);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn pause_changed_per_rule_targets_only_that_rule_and_global_is_unscoped() {
        let first = DisplayId("first".into());
        let second = DisplayId("second".into());
        let (mut engine, mut events) = subscribed_manual_engine(first.clone());
        let timings = DisplayRuntimeCfg::manual_defaults(Duration::ZERO);
        let ladder = vec![LadderStage {
            kind: StageKind::Controller(BlankMode::PowerOff),
            dwell: None,
        }];
        let machine = DisplayStateMachine::new(timings, ladder, Tick::now());
        engine.machines.insert(second.clone(), machine);
        engine.cfg.displays.push(DisplayRuntimeCfg {
            display: second.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![LadderStage {
                kind: StageKind::Controller(BlankMode::PowerOff),
                dwell: None,
            }],
            timings: DisplayRuntimeCfg::manual_defaults(Duration::ZERO),
        });
        let rule_a = RuleId("rule-a".into());
        let rule_b = RuleId("rule-b".into());
        engine
            .rule_displays
            .insert(rule_a.clone(), vec![first.clone()]);
        engine.rule_displays.insert(rule_b, vec![second.clone()]);
        assert!(
            engine.machines.contains_key(&second),
            "other rule display exists"
        );

        engine.handle_pause(Some(&rule_a), None);
        assert!(matches!(
            events.try_recv(),
            Ok(DaemonEvent::PauseChanged { display, paused: true, rule: Some(rule) })
                if display == first && rule == rule_a
        ));
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        engine.handle_pause(None, None);
        let global_event = events
            .try_recv()
            .expect("global pause reached other display");
        assert!(
            matches!(
                global_event,
                DaemonEvent::PauseChanged { ref display, paused: true, rule: None }
                    if *display == second
            ),
            "unexpected global event: {global_event:?}"
        );
    }

    #[test]
    fn pause_changed_wire_tag_and_unknown_are_forward_compatible() {
        let event = DaemonEvent::PauseChanged {
            display: DisplayId("mon".into()),
            paused: true,
            rule: None,
        };
        let json = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(json["event"], "pause_changed");
        let unknown: DaemonEvent =
            serde_json::from_str(r#"{"event":"future_event"}"#).expect("unknown event tolerated");
        assert!(matches!(unknown, DaemonEvent::Unknown));
    }

    #[test]
    fn old_wear_snapshot_without_attribution_mode_deserializes_as_uniform() {
        let old =
            r#"{"event":"wear_snapshot","display":"desk","total_on_hours":1.5,"sample_count":3}"#;
        let event: DaemonEvent = serde_json::from_str(old).unwrap();
        assert!(matches!(
            event,
            DaemonEvent::WearSnapshot {
                wear_attribution_mode: crate::wear::WearAttributionMode::Uniform,
                ..
            }
        ));
    }

    /// The daemon must never construct `DaemonEvent::Unknown` — the ctl
    /// handler debug-asserts this at the publish seam so a bug that tries
    /// fails loudly in debug builds instead of silently shipping a
    /// meaningless event.
    #[test]
    #[should_panic(expected = "daemon must never construct Unknown")]
    fn publish_unknown_event_debug_asserts() {
        let mut engine = minimal_engine();
        engine.handle_control(ControlMsg::PublishDaemonEvent(DaemonEvent::Unknown));
    }

    // ── MQTT availability gates the stale sweep (issue #136) ───────────────

    use crate::types::SensorAvailabilityEvent;

    /// A minimal engine with exactly ONE sensor and a SHORT `stale_timeout`
    /// — for the availability-gating tests below, which need a sweep to fire
    /// within a few hundred milliseconds of virtual time.
    fn engine_with_short_stale(id: &str, stale: Duration) -> RulesEngine {
        let sid = SensorId(id.into());
        RulesEngine::new(
            RulesEngineConfig {
                rules: vec![],
                displays: vec![],
                sensors: vec![SensorRuntimeCfg {
                    sensor: sid.clone(),
                    kind: SensorKind::Presence,
                    hold_time: None,
                    stale_timeout: stale,
                }],
                doctor_wake_settle: Duration::from_secs(3),
            },
            ZoneEngine::new(vec![], &[sid]).expect("single-sensor empty-zone engine is valid"),
            HashMap::new(),
            HashMap::new(),
            Arc::new(crate::ownership::AlwaysOwned),
        )
        .expect("single-sensor engine config is valid")
    }

    /// Helper: find the sensor snapshot for `id` and return its
    /// `(state, last_seen_secs_ago)`.
    fn sensor_view(snap: &StateSnapshot, id: &str) -> (SensorState, u64) {
        let s = snap
            .sensors
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("sensor {id} in snapshot"));
        (s.state, s.last_seen_secs_ago)
    }

    /// Issue #136 + #205: a healthy radar with a retained `online`
    /// availability signal must not be marked `Unavailable` by the stale
    /// sweep just because the state topic is silent (occupant seated, no
    /// motion) — AS LONG AS the silence is bounded by the sensor's
    /// configured `stale_timeout`. The online assertion is a LEASE: it
    /// gates the sweep for `stale_timeout` after the assertion, then
    /// expires so a sensor that goes truly silent cannot preserve stale
    /// presence forever. The online event also must NOT refresh the
    /// `last_seen` clock — silence still means "state unchanged", not
    /// "new data".
    #[tokio::test(start_paused = true)]
    async fn retained_online_suppresses_silence_staleness() {
        let sensor = SensorId("desk".into());
        let mut engine = engine_with_short_stale("desk", Duration::from_millis(500));

        // 1. Sensor reports Present.
        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Present,
            Timestamp::now(),
        ));

        // Snapshot the last-seen baseline so we can prove the online
        // assertion did NOT refresh it.
        let baseline = snapshot_of(&mut engine);
        let (_, last_seen_baseline) = sensor_view(&baseline, "desk");
        assert_eq!(last_seen_baseline, 0, "no virtual time has passed yet");

        // 2. Sensor source asserts online.
        engine.handle_control(ControlMsg::SensorAvailability(
            SensorAvailabilityEvent::new(sensor.clone(), true, Timestamp::now()),
        ));

        // 3. Advance virtual time by LESS than `stale_timeout` (the lease
        //    is still valid), then run the sweep explicitly (these tests
        //    drive `handle_*` directly; the `run()` loop's timer-driven
        //    sweep is exercised by `tests/rules_end_to_end.rs`).
        tokio::time::sleep(Duration::from_millis(200)).await;
        engine.sweep_stale_sensors();

        // The state must still be Present — the online lease is still
        // valid, so silence is "state unchanged".
        let after = snapshot_of(&mut engine);
        let (state, last_seen_after) = sensor_view(&after, "desk");
        assert_eq!(
            state,
            SensorState::Present,
            "online lease below stale_timeout must keep a present sensor present across silence (got {state:?})"
        );
        // CRUCIAL: last_seen must reflect elapsed time (not be reset by
        // online). The 200ms sleep elapses, so the unscaled
        // `last_seen_secs_ago` must be > the baseline — proving the online
        // event did not refresh the clock.
        assert!(
            last_seen_after >= last_seen_baseline,
            "last_seen_secs_ago must reflect elapsed time, not be reset by online"
        );
    }

    /// Issue #205: the `availability_online` marker is a lease bounded by
    /// `stale_timeout`. Once the lease expires, the next stale sweep must
    /// demote the sensor to `Unavailable` so a dead connection whose last
    /// broker-side state was `online` cannot preserve stale presence
    /// forever. The zone's fail-safe-present policy keeps the display
    /// awake — the sensor state still moves through `Unavailable`, never
    /// to `Absent` (repo rule 6).
    #[tokio::test(start_paused = true)]
    async fn availability_online_lease_expires_after_stale_timeout() {
        let sensor = SensorId("lease".into());
        let mut engine = engine_with_short_stale("lease", Duration::from_millis(500));

        // Present, then online assertion at t=0.
        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Present,
            Timestamp::now(),
        ));
        engine.handle_control(ControlMsg::SensorAvailability(
            SensorAvailabilityEvent::new(sensor.clone(), true, Timestamp::now()),
        ));

        // Below the lease: state stays Present.
        tokio::time::sleep(Duration::from_millis(200)).await;
        engine.sweep_stale_sensors();
        let mid = snapshot_of(&mut engine);
        assert_eq!(
            sensor_view(&mid, "lease").0,
            SensorState::Present,
            "online lease must keep the sensor present below stale_timeout"
        );

        // Advance past stale_timeout. The lease expires; the next sweep
        // demotes the sensor to Unavailable.
        tokio::time::sleep(Duration::from_millis(400)).await;
        engine.sweep_stale_sensors();
        let after = snapshot_of(&mut engine);
        let (state, _) = sensor_view(&after, "lease");
        assert_eq!(
            state,
            SensorState::Unavailable,
            "online lease must expire after stale_timeout and demote the sensor to Unavailable (got {state:?})"
        );
        // Crucially NOT Absent — repo rule 6 forbids data loss = absent.
        assert_ne!(
            state,
            SensorState::Absent,
            "silence must never be reported as Absent — only Unavailable (repo rule 6)"
        );
    }

    /// A sensor with NO availability topic keeps today's behavior: silence
    /// past `stale_timeout` marks it `Unavailable`. This is the regression
    /// guard for sensors whose bridge does not publish LWT.
    #[tokio::test(start_paused = true)]
    async fn no_availability_still_stales() {
        let sensor = SensorId("no_aw".into());
        let mut engine = engine_with_short_stale("no_aw", Duration::from_millis(500));

        // Present, no availability event. Sweep at 1.2s should mark
        // Unavailable — the pre-#136 behavior.
        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Present,
            Timestamp::now(),
        ));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        engine.sweep_stale_sensors();

        let after = snapshot_of(&mut engine);
        let (state, _) = sensor_view(&after, "no_aw");
        assert_eq!(
            state,
            SensorState::Unavailable,
            "sensor without an availability topic must still go stale on silence (got {state:?})"
        );
    }

    /// An explicit `offline` availability edge must mark the sensor
    /// `Unavailable` IMMEDIATELY, even if the stale sweep hasn't fired
    /// yet — that's the LWT contract.
    #[tokio::test(start_paused = true)]
    async fn explicit_offline_transitions_to_unavailable_immediately() {
        let sensor = SensorId("lwt".into());
        let mut engine = engine_with_short_stale("lwt", Duration::from_secs(3600));

        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Present,
            Timestamp::now(),
        ));
        engine.handle_control(ControlMsg::SensorAvailability(
            SensorAvailabilityEvent::new(sensor.clone(), true, Timestamp::now()),
        ));

        // LWT fires well before the long stale timeout would.
        engine.handle_control(ControlMsg::SensorAvailability(
            SensorAvailabilityEvent::new(sensor.clone(), false, Timestamp::now()),
        ));

        let snap = snapshot_of(&mut engine);
        let (state, _) = sensor_view(&snap, "lwt");
        assert_eq!(
            state,
            SensorState::Unavailable,
            "explicit offline must transition to Unavailable immediately (got {state:?})"
        );
    }

    /// Broker disconnect (a `PresenceEvent::Unavailable` from the source)
    /// must clear the `availability_online` lease so a dead connection
    /// cannot preserve stale presence forever — the next sweep then
    /// correctly demotes a still-quiet sensor.
    #[tokio::test(start_paused = true)]
    async fn broker_disconnect_clears_online_assertion() {
        let sensor = SensorId("disc".into());
        let mut engine = engine_with_short_stale("disc", Duration::from_millis(500));

        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Present,
            Timestamp::now(),
        ));
        engine.handle_control(ControlMsg::SensorAvailability(
            SensorAvailabilityEvent::new(sensor.clone(), true, Timestamp::now()),
        ));

        // Advance time UNDER the online lease (stale_timeout = 500ms):
        // state stays Present.
        tokio::time::sleep(Duration::from_millis(200)).await;
        engine.sweep_stale_sensors();
        let still_present = snapshot_of(&mut engine);
        assert_eq!(sensor_view(&still_present, "disc").0, SensorState::Present);

        // Broker drops: source emits Unavailable. The online lease
        // must clear so the next sweep re-engages the timeout.
        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Unavailable,
            Timestamp::now(),
        ));

        // Now bring the sensor back to Present (reconnect restored the
        // subscription), but WITHOUT a fresh online assertion. Advance
        // past the stale timeout: the sweep must demote it again
        // because the disconnect cleared the assertion.
        engine.handle_presence_event(PresenceEvent::new(
            sensor.clone(),
            SensorState::Present,
            Timestamp::now(),
        ));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        engine.sweep_stale_sensors();

        let after = snapshot_of(&mut engine);
        let (state, _) = sensor_view(&after, "disc");
        assert_eq!(
            state,
            SensorState::Unavailable,
            "post-disconnect silence must re-engage the stale sweep (got {state:?})"
        );
    }
}

/// Restoring a manual-only display's phase into the engine must carry
/// the restored machine and its scheduling effects into the engine's
/// internal structures — no phantoms, no dropped machines.
#[test]
fn install_restored_machine_replaces_phase_and_queues_effects() {
    use crate::ownership::AlwaysOwned;
    use crate::state_machine::Phase;
    use std::collections::BinaryHeap;

    let display_id = DisplayId("test-disp".into());
    let now = Tick::now();
    let timings = SmTimings {
        grace_period: Duration::from_secs(60),
        min_blank_time: Duration::from_secs(0),
        min_wake_time: Duration::from_secs(0),
        startup_holdoff: Duration::from_secs(10),
        wake_retry_interval: Duration::from_secs(60),
    };
    let ladder = vec![LadderStage {
        kind: StageKind::Controller(BlankMode::PowerOff),
        dwell: None,
    }];

    // Build a minimal RulesEngine with one Active machine.
    let machine = DisplayStateMachine::new(timings.clone(), ladder.clone(), now);
    let mut machines = HashMap::new();
    machines.insert(display_id.clone(), machine);
    let zone_engine = ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid");
    let (results_tx, results_rx) = mpsc::unbounded_channel();
    let (event_tx, _) = broadcast::channel(256);

    let mut engine = RulesEngine {
        cfg: RulesEngineConfig {
            rules: vec![],
            displays: vec![],
            sensors: vec![],
            doctor_wake_settle: Duration::from_secs(3),
        },
        zone_engine,
        machines,
        executors: HashMap::new(),
        render_sinks: HashMap::new(),
        ownership: Arc::new(AlwaysOwned),
        coordination: None,
        last_owned: HashMap::new(),
        rule_displays: HashMap::new(),
        zone_rules: HashMap::new(),
        paused_rules: HashSet::new(),
        paused_scopes: HashMap::new(),
        inhibitor_state: HashMap::new(),
        holds: HashMap::new(),
        wake_attempts: HashMap::new(),
        reported: HashSet::new(),
        last_blank_failed: HashSet::new(),
        sensor_last_seen_virtual: HashMap::new(),
        availability_online: HashMap::new(),
        timers: BinaryHeap::new(),
        results_rx,
        results_tx,
        event_tx,
        observations: None,
        operation_registry: OperationRegistry::default(),
        pending_reload: None,
        rollback: None,
        kvm: None,
        claim_suppression: HashMap::new(),
        input_wake_holds: HashMap::new(),
        pending_restore: Vec::new(),
    };

    // Restore a machine to Blanked — a manual-only display's phase
    // from before a reload.
    let (restored, effects) = DisplayStateMachine::restore(timings, ladder, Phase::Blanked, 1, now);
    // Phase::Blanked restore emits no scheduling effects.
    assert!(effects.is_empty());

    // Act — install the restored machine.
    engine.install_restored_machine(&display_id, restored, effects, now);

    // Assert: the engine's machine is now Blanked (not the original Active).
    let machine = engine.machines.get(&display_id).unwrap();
    assert_eq!(*machine.phase(), Phase::Blanked);

    // Assert: the restore was queued — one entry keyed to our display.
    assert_eq!(engine.pending_restore.len(), 1);
    assert_eq!(engine.pending_restore[0].0, display_id);
    // Restoring to Blanked emits no IssueWake or IssueBlank effects.
    for effect in &engine.pending_restore[0].1 {
        assert!(
            !matches!(effect, Effect::IssueBlank { .. } | Effect::IssueWake { .. }),
            "Blanked restore must not emit blank/wake effects, got {effect:?}"
        );
    }

    // Assert: ownership was re-seeded (AlwaysOwned returns true).
    assert_eq!(engine.last_owned.get(&display_id), Some(&true));
}

/// Pins that the ownership re-feed in `install_restored_machine` runs and
/// its effects are routed into `pending_restore` (not dropped).  With a
/// `NeverOwned` gate, restoring a Blanked machine must yield ownership →
/// enter Active (phase change proves the re-feed ran) and emit a
/// `LogTransition` (effect-queued proves effects weren't dropped).
#[test]
fn install_restored_never_owned_refeed_not_dropped() {
    use crate::ownership::OwnershipGate;
    use crate::state_machine::Phase;
    use std::collections::BinaryHeap;

    // Test-only gate that never claims ownership.
    struct NeverOwned;
    impl OwnershipGate for NeverOwned {
        fn owns(&self, _display: &DisplayId) -> bool {
            false
        }
    }

    let display_id = DisplayId("test-disp".into());
    let now = Tick::now();
    let timings = SmTimings {
        grace_period: Duration::from_secs(60),
        min_blank_time: Duration::from_secs(0),
        min_wake_time: Duration::from_secs(0),
        startup_holdoff: Duration::from_secs(10),
        wake_retry_interval: Duration::from_secs(60),
    };
    let ladder = vec![LadderStage {
        kind: StageKind::Controller(BlankMode::PowerOff),
        dwell: None,
    }];

    // Build a minimal RulesEngine with one Active machine.
    let machine = DisplayStateMachine::new(timings.clone(), ladder.clone(), now);
    let mut machines = HashMap::new();
    machines.insert(display_id.clone(), machine);
    let zone_engine = ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid");
    let (results_tx, results_rx) = mpsc::unbounded_channel();
    let (event_tx, _) = broadcast::channel(256);

    let mut engine = RulesEngine {
        cfg: RulesEngineConfig {
            rules: vec![],
            displays: vec![],
            sensors: vec![],
            doctor_wake_settle: Duration::from_secs(3),
        },
        zone_engine,
        machines,
        executors: HashMap::new(),
        render_sinks: HashMap::new(),
        ownership: Arc::new(NeverOwned),
        coordination: None,
        last_owned: HashMap::new(),
        rule_displays: HashMap::new(),
        zone_rules: HashMap::new(),
        paused_rules: HashSet::new(),
        paused_scopes: HashMap::new(),
        inhibitor_state: HashMap::new(),
        holds: HashMap::new(),
        wake_attempts: HashMap::new(),
        reported: HashSet::new(),
        last_blank_failed: HashSet::new(),
        sensor_last_seen_virtual: HashMap::new(),
        availability_online: HashMap::new(),
        timers: BinaryHeap::new(),
        results_rx,
        results_tx,
        event_tx,
        observations: None,
        operation_registry: OperationRegistry::default(),
        pending_reload: None,
        rollback: None,
        kvm: None,
        claim_suppression: HashMap::new(),
        input_wake_holds: HashMap::new(),
        pending_restore: Vec::new(),
    };

    // Restore a machine to Blanked — a manual-only display's phase
    // from before a reload.
    let (restored, restore_effects) =
        DisplayStateMachine::restore(timings, ladder, Phase::Blanked, 1, now);
    assert!(restore_effects.is_empty());

    // Act — install the restored machine.
    engine.install_restored_machine(&display_id, restored, restore_effects, now);

    // Assert: the re-feed RAN — owns:false on Blanked transitions to Active
    // via enter_active("ownership_yielded").
    let machine = engine.machines.get(&display_id).unwrap();
    assert_eq!(
        *machine.phase(),
        Phase::Active,
        "owns:false on Blanked must yield ownership → Active (re-feed ran)"
    );

    // Assert: the re-feed effects are queued (NOT dropped).
    // Blanked + OwnershipChanged(false) emits LogTransition via enter_active.
    assert_eq!(engine.pending_restore.len(), 1);
    let queued = &engine.pending_restore[0].1;
    let has_transition = queued
        .iter()
        .any(|e| matches!(e, Effect::LogTransition { .. }));
    assert!(
        has_transition,
        "refeed LogTransition must be queued, got {queued:?}"
    );

    // Assert: ownership was re-seeded (NeverOwned returns false).
    assert_eq!(engine.last_owned.get(&display_id), Some(&false));
}

#[cfg(test)]
use crate::fakes::RecordingSink;

#[cfg(test)]
/// Build a test engine with one display, one zone, one presence sensor,
/// and one rule whose `input_wake_hold` is `hold`.  The `RecordingSink`
/// records blank/wake commands and succeeds by default.
fn input_wake_hold_engine(hold: Duration) -> (RulesEngine, DisplayId, Arc<RecordingSink>) {
    use crate::zone::{FusionMode, ZoneMember, ZoneSpec};
    let display = DisplayId("d1".into());
    let zone = ZoneId("z1".into());
    let sensor = SensorId("s1".into());

    let timings = DisplayRuntimeCfg::manual_defaults(Duration::ZERO);
    let ladder = vec![LadderStage {
        kind: StageKind::Controller(BlankMode::PowerOff),
        dwell: None,
    }];

    let sink = Arc::new(RecordingSink::new());
    let mut executors = HashMap::new();
    executors.insert(display.clone(), sink.clone() as Arc<dyn CommandSink>);

    let zone_spec = ZoneSpec {
        id: zone.clone(),
        mode: FusionMode::Any,
        members: vec![ZoneMember::Sensor(sensor.clone())],
        weights: HashMap::new(),
        unavailable_policy: crate::zone::UnavailablePolicy::Present,
    };

    let engine = RulesEngine::new(
        RulesEngineConfig {
            rules: vec![RuleRuntimeCfg {
                rule: RuleId("r1".into()),
                zone: zone.clone(),
                displays: vec![display.clone()],
                input_wake_hold: hold,
            }],
            displays: vec![DisplayRuntimeCfg {
                display: display.clone(),
                blank_mode: BlankMode::PowerOff,
                ladder: ladder.clone(),
                timings,
            }],
            sensors: vec![SensorRuntimeCfg {
                sensor: sensor.clone(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: Duration::from_secs(3600),
            }],
            doctor_wake_settle: Duration::from_secs(3),
        },
        ZoneEngine::new(vec![zone_spec], &[sensor]).expect("zone engine must be valid"),
        executors,
        HashMap::new(),
        Arc::new(crate::ownership::AlwaysOwned),
    )
    .expect("engine must be valid");

    (engine, display, sink)
}

#[test]
fn input_wake_hold_requires_all_driving_zones_vacant() {
    use crate::zone::{FusionMode, ZoneMember, ZoneSpec};

    let display = DisplayId("d1".into());
    let first_zone = ZoneId("z1".into());
    let second_zone = ZoneId("z2".into());
    let first_sensor = SensorId("s1".into());
    let second_sensor = SensorId("s2".into());
    let sink = Arc::new(RecordingSink::new());
    let mut executors = HashMap::new();
    executors.insert(display.clone(), sink as Arc<dyn CommandSink>);
    let cfg = RulesEngineConfig {
        rules: vec![
            RuleRuntimeCfg {
                rule: RuleId("r1".into()),
                zone: first_zone.clone(),
                displays: vec![display.clone()],
                input_wake_hold: Duration::from_secs(120),
            },
            RuleRuntimeCfg {
                rule: RuleId("r2".into()),
                zone: second_zone.clone(),
                displays: vec![display.clone()],
                input_wake_hold: Duration::from_secs(30),
            },
        ],
        displays: vec![DisplayRuntimeCfg {
            display: display.clone(),
            blank_mode: BlankMode::PowerOff,
            ladder: vec![],
            timings: DisplayRuntimeCfg::manual_defaults(Duration::ZERO),
        }],
        sensors: vec![
            SensorRuntimeCfg {
                sensor: first_sensor.clone(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: Duration::from_secs(3600),
            },
            SensorRuntimeCfg {
                sensor: second_sensor.clone(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: Duration::from_secs(3600),
            },
        ],
        doctor_wake_settle: Duration::from_secs(3),
    };
    let mut engine = RulesEngine::new(
        cfg,
        ZoneEngine::new(
            vec![
                ZoneSpec {
                    id: first_zone.clone(),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Sensor(first_sensor.clone())],
                    weights: HashMap::new(),
                    unavailable_policy: crate::zone::UnavailablePolicy::Present,
                },
                ZoneSpec {
                    id: second_zone.clone(),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Sensor(second_sensor.clone())],
                    weights: HashMap::new(),
                    unavailable_policy: crate::zone::UnavailablePolicy::Present,
                },
            ],
            &[first_sensor.clone(), second_sensor.clone()],
        )
        .expect("zone engine must be valid"),
        executors,
        HashMap::new(),
        Arc::new(crate::ownership::AlwaysOwned),
    )
    .expect("engine must be valid");

    engine.zone_engine.apply(&PresenceEvent::new(
        first_sensor,
        SensorState::Absent,
        Timestamp::now(),
    ));
    assert_eq!(engine.zone_engine.is_present(&first_zone), Some(false));
    engine.zone_engine.apply(&PresenceEvent::new(
        second_sensor,
        SensorState::Present,
        Timestamp::now(),
    ));
    assert_eq!(engine.zone_engine.is_present(&second_zone), Some(true));

    assert_eq!(engine.effective_input_wake_hold(&display), None);
}

#[cfg(test)]
/// Drive a display machine through the Happy Path to Blanked:
/// zone absent → Grace → tick expiry → Blanking → BlankResult(Ok) → Blanked.
fn drive_to_blanked(engine: &mut RulesEngine, display: &DisplayId) {
    let now = Tick::now();
    // Mark zone absent via a presence event.
    engine.handle_presence_event(PresenceEvent::new(
        SensorId("s1".into()),
        SensorState::Absent,
        Timestamp::now(),
    ));
    // Drive grace expiry by stepping with a Tick past the grace period.
    let grace_end = Tick(now.0 + Duration::from_secs(60)); // manual_defaults uses 60s grace
    engine.step_machine(display, Input::Tick, grace_end);
    // Now the machine should be in Blanking; feed a successful BlankResult.
    let blank_gen = engine
        .machines
        .get(display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        display,
        Input::BlankResult {
            r#gen: blank_gen,
            result: Ok(()),
        },
        Tick::now(),
    );
}

/// After a display-wake from Blanked (issue #125), the input-wake hold keeps
/// the display in Active — it does NOT immediately re-enter Grace even though
/// the zone is known vacant.
#[tokio::test(start_paused = true)]
async fn vacant_blanked_input_wake_stays_active_inside_hold() {
    let (mut engine, display, _sink) = input_wake_hold_engine(Duration::from_secs(120));

    drive_to_blanked(&mut engine, &display);

    // The display is now Blanked; the zone is vacant.
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "blanked"
    );

    // Act: send InputWake (simulating operator typing on the render surface).
    engine.handle_control(ControlMsg::InputWake(display.clone()));

    // Feed a successful WakeResult to complete the wake.
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );

    // Assert: the hold is armed.
    assert!(
        engine.input_wake_holds.contains_key(&display),
        "input_wake_hold must be armed for display after InputWake in vacant zone"
    );

    // Assert: the state machine is Active (not Grace).
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "active",
        "display must stay Active inside the hold — not re-enter Grace"
    );
}

/// After the input-wake hold expires in a still-vacant zone, the display
/// must re-enter Grace (never a direct blank).
#[tokio::test(start_paused = true)]
async fn vacant_blanked_input_wake_reenters_grace_after_hold() {
    let (mut engine, display, _sink) = input_wake_hold_engine(Duration::from_secs(120));

    drive_to_blanked(&mut engine, &display);

    // Send InputWake and complete the wake.
    engine.handle_control(ControlMsg::InputWake(display.clone()));
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );

    // Assert hold is armed.
    assert!(engine.input_wake_holds.contains_key(&display));

    // Advance time past the hold (120 s).
    tokio::time::sleep(Duration::from_secs(121)).await;

    // Fire due timers — the InputWakeHoldExpiry should fire.
    engine.fire_due_timers(Tick::now());

    // Assert: hold was removed.
    assert!(
        !engine.input_wake_holds.contains_key(&display),
        "hold must be cleared after expiry"
    );

    // Assert: the state machine is now in Grace (never a direct blank).
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "grace",
        "expired hold must re-enter Grace, never a direct blank"
    );
}

/// A zone can become present without the display seeing the edge before the
/// hold timer fires; expiry must re-check the zones before re-entering Grace.
#[tokio::test(start_paused = true)]
async fn presence_at_input_wake_hold_expiry_does_not_reblank() {
    let (mut engine, display, sink) = input_wake_hold_engine(Duration::from_secs(120));

    drive_to_blanked(&mut engine, &display);
    engine.handle_control(ControlMsg::InputWake(display.clone()));
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );

    engine.zone_engine.apply(&PresenceEvent::new(
        SensorId("s1".into()),
        SensorState::Present,
        Timestamp::now(),
    ));
    let deadline = *engine
        .input_wake_holds
        .get(&display)
        .expect("input-wake hold must be armed");
    engine.fire_input_wake_hold_expiry(&display, Tick(deadline));

    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "active"
    );
    assert!(
        !sink
            .log()
            .iter()
            .any(|(_, command)| matches!(command, crate::fakes::SinkCmd::Blank(_)))
    );
}

/// When presence returns during an input-wake hold, the hold must be
/// cancelled — the display stays awake normally as long as the room is
/// occupied, without a latent timer that re-blanks it.
#[tokio::test(start_paused = true)]
async fn presence_during_input_wake_hold_cancels_reblank() {
    let (mut engine, display, _sink) = input_wake_hold_engine(Duration::from_secs(120));

    drive_to_blanked(&mut engine, &display);

    // Send InputWake and complete the wake.
    engine.handle_control(ControlMsg::InputWake(display.clone()));
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );

    // Assert hold is armed.
    assert!(engine.input_wake_holds.contains_key(&display));

    // Act: presence returns.
    engine.handle_presence_event(PresenceEvent::new(
        SensorId("s1".into()),
        SensorState::Present,
        Timestamp::now(),
    ));

    // Assert: hold was cleared by presence.
    assert!(
        !engine.input_wake_holds.contains_key(&display),
        "hold must be cleared when presence returns"
    );

    // Assert: the state machine is Active (presence detected).
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "active",
        "presence during hold must keep display Active"
    );

    // Advance time past the original hold deadline to prove no latent
    // timer re-blanks an occupied room.
    tokio::time::sleep(Duration::from_secs(121)).await;
    engine.fire_due_timers(Tick::now());

    // Still Active — the hold is gone, presence keeps it awake.
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "active",
        "no latent timer must re-blank an occupied room after hold cleared"
    );
}

/// When `input_wake_hold` is `0s`, the display immediately re-enters Grace
/// after an input wake in a vacant zone — the pre-#125 behaviour.
#[tokio::test(start_paused = true)]
async fn zero_input_wake_hold_preserves_immediate_grace_behavior() {
    let (mut engine, display, _sink) = input_wake_hold_engine(Duration::ZERO);

    drive_to_blanked(&mut engine, &display);

    // Send InputWake and complete the wake with hold=0.
    engine.handle_control(ControlMsg::InputWake(display.clone()));
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );

    // Assert: no hold was armed (hold == 0s disables it).
    assert!(
        !engine.input_wake_holds.contains_key(&display),
        "zero input_wake_hold must not arm a hold"
    );

    // Assert: the state machine is already in Grace (immediate re-grace).
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "grace",
        "zero hold must immediately re-enter Grace after InputWake in vacant zone"
    );
}

/// A second `InputWake` during an active hold re-arms the deadline — the
/// old timer must NOT prematurely clear the hold.  This pins the stale-
/// timer guard in `fire_input_wake_hold_expiry`.
#[tokio::test(start_paused = true)]
async fn second_input_wake_rearms_hold() {
    let (mut engine, display, _sink) = input_wake_hold_engine(Duration::from_secs(120));

    drive_to_blanked(&mut engine, &display);

    // Drain any stale timers left by drive_to_blanked (e.g. DisplayTick
    // from the initial Grace entry) so they don't interfere with the
    // hold-timer assertions below.
    engine.fire_due_timers(Tick::now());

    // --- First InputWake: arm the hold (deadline = now + 120s) ---
    engine.handle_control(ControlMsg::InputWake(display.clone()));
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );
    assert!(engine.input_wake_holds.contains_key(&display));
    // Verify we're active (hold prevents deferred Grace).
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "active"
    );

    // --- Advance half the hold (60s) ---
    tokio::time::sleep(Duration::from_secs(60)).await;

    // --- Second InputWake: re-arm the hold (deadline = now + 120s = 180s) ---
    engine.handle_control(ControlMsg::InputWake(display.clone()));
    let wake_gen = engine
        .machines
        .get(&display)
        .map_or(0, DisplayStateMachine::cmd_gen);
    engine.step_machine(
        &display,
        Input::WakeResult {
            r#gen: wake_gen,
            result: Ok(()),
        },
        Tick::now(),
    );

    // --- Advance past the FIRST deadline (120s from start) ---
    // At t=60s we re-armed, so now we're at t=60+61=121s, past the
    // original 120s deadline.  The old timer fires here.
    tokio::time::sleep(Duration::from_secs(61)).await;
    engine.fire_due_timers(Tick::now());

    // Assert: STILL active — the old timer must NOT have cleared the
    // re-armed hold.
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "active",
        "old timer must not clear a re-armed hold"
    );
    assert!(
        engine.input_wake_holds.contains_key(&display),
        "hold must still be armed after stale timer fires"
    );

    // --- Advance just past the SECOND deadline (180s from start; we are at
    // t=121s, so +60s lands at t=181s). Overshooting further would also
    // expire the grace period that starts at expiry, collapsing
    // grace→blanking inside one timer drain and masking the phase we
    // assert here. ---
    tokio::time::sleep(Duration::from_secs(60)).await;
    engine.fire_due_timers(Tick::now());

    // Assert: now in Grace — the second timer fired correctly.
    assert!(
        !engine.input_wake_holds.contains_key(&display),
        "hold must be cleared after second deadline"
    );
    assert_eq!(
        engine.machines.get(&display).unwrap().phase_name(),
        "grace",
        "re-armed hold expiry must re-enter Grace"
    );
}
