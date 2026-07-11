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
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::SensorKind;
use crate::error::DormantError;
use crate::ownership::OwnershipGate;
use crate::state_machine::{DisplayStateMachine, Effect, Input, SmTimings};
use crate::traits::{CommandSink, PanelState, RenderSink};
use crate::types::{
    BlankMode, CmdFailure, DisplayId, LadderStage, PresenceEvent, RuleId, SensorId, SensorState,
    StageKind, Tick, Timestamp, ZoneId,
};
use crate::zone::ZoneEngine;

// ── Public I/O surfaces ───────────────────────────────────────────────────────

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
        /// Whether the inhibitor is now engaged.
        inhibited: bool,
    },
    /// Set or clear the pending-reload indicator at runtime (operator feedback
    /// in [`StateSnapshot`]s). Lets the daemon flag a rejected reload without
    /// tearing down the running engine.
    SetPendingReload(Option<String>),
    /// Input-wake event from the render surface — route to the display
    /// machine's [`Input::InputWake`].
    InputWake(DisplayId),
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
    },
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
    /// Last ownership value fed per display — change-detection to avoid
    /// redundant [`Input::OwnershipChanged`] edges every tick.
    last_owned: HashMap<DisplayId, bool>,
    /// Rule → its displays.
    rule_displays: HashMap<RuleId, Vec<DisplayId>>,
    /// Zone → rules bound to it.
    zone_rules: HashMap<ZoneId, Vec<RuleId>>,
    /// Sensors that are currently paused at the rule level (skip blanking).
    paused_rules: HashSet<RuleId>,
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
    /// Timer wheel — min-heap on `(Tick, entry)`.
    timers: BinaryHeap<Reverse<(Tick, TimerEntry)>>,
    /// Internal results mpsc — spawned dispatch tasks write here.
    results_rx: mpsc::UnboundedReceiver<InternalResult>,
    /// Internal results mpsc — cloned into each spawned dispatch task.
    results_tx: mpsc::UnboundedSender<InternalResult>,
    /// Broadcast bus for [`DaemonEvent`]s.
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Pending reload detail (operator feedback in snapshots).
    pending_reload: Option<String>,
    /// Pre-run effects queued by [`RulesEngine::apply_restore_effects`].
    /// Drained into timer / dispatch structures at the start of
    /// [`RulesEngine::run`].
    pending_restore: Vec<(DisplayId, Vec<Effect>)>,
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
            last_owned,
            rule_displays,
            zone_rules,
            paused_rules: HashSet::new(),
            holds,
            wake_attempts: HashMap::new(),
            reported: HashSet::new(),
            last_blank_failed: HashSet::new(),
            sensor_last_seen_virtual: HashMap::new(),
            timers: BinaryHeap::new(),
            results_rx,
            results_tx,
            event_tx,
            pending_reload: None,
            pending_restore: Vec::new(),
        })
    }

    /// Set or clear the pending-reload indicator (operator feedback in
    /// [`StateSnapshot`]s).
    pub fn set_pending_reload(&mut self, detail: Option<String>) {
        self.pending_reload = detail;
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
        if let Some(slot) = self.machines.get_mut(display) {
            *slot = machine;
            let owns = self.ownership.owns(display);
            let refeed = slot.step(Input::OwnershipChanged(owns), now);
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

        // Diagnostic-only: record that this sensor has reported at least
        // once since daemon start — ANY state (Present/Absent/Unavailable)
        // counts as "has reported".  Deliberately separate from
        // `sensor_last_seen_virtual` above (see the field doc on
        // `RulesEngine::reported`).
        self.reported.insert(effective.sensor_id.clone());

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
                // Feed ownership so the gate verdict is current before
                // processing the presence edge.
                let own_effects = self.feed_ownership(&display_id, now);
                for effect in own_effects {
                    self.process_effect(&display_id, effect);
                }
                let Some(machine) = self.machines.get_mut(&display_id) else {
                    continue;
                };
                let effects = machine.step(Input::ZonePresent(present), now);
                for effect in effects {
                    self.process_effect(&display_id, effect);
                }
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
            ControlMsg::InputWake(d) => self.step_one(&d, Input::InputWake),
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
            ControlMsg::SetInhibited { rule, inhibited } => {
                self.handle_set_inhibited(rule.as_ref(), inhibited);
            }
            ControlMsg::SetPendingReload(detail) => self.set_pending_reload(detail),
            ControlMsg::EmergencyWake { reply } => self.handle_emergency_wake(reply),
            ControlMsg::Exercise { display, reply } => self.handle_exercise(display, reply),
        }
    }

    /// Route an inhibitor state change to a rule's displays (or every display
    /// when `rule` is `None`), mirroring [`Self::handle_pause`]'s fan-out.
    fn handle_set_inhibited(&mut self, rule: Option<&RuleId>, inhibited: bool) {
        let targets: Vec<DisplayId> = match rule {
            Some(r) => self.rule_displays.get(r).cloned().unwrap_or_default(),
            None => self
                .cfg
                .displays
                .iter()
                .map(|d| d.display.clone())
                .collect(),
        };
        for d in targets {
            self.step_one(&d, Input::InhibitorChanged(inhibited));
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
    fn handle_emergency_wake(&mut self, reply: oneshot::Sender<EmergencyWakeReport>) {
        // Pause every rule indefinitely — reuse the existing pause path so
        // the state-machine overlays route the same way as a normal
        // global pause.  Without this an absent-zone would re-trigger
        // blanking on the very next sensor event.
        self.handle_pause(None, None);

        // Snapshot executor handles so the spawned task does not hold a
        // borrow on `self`.  Per the spec, wake EVERY display the engine
        // owns — that includes manual-only displays (no rule bound).
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
            // Spawn ALL per-display wake tasks up front, THEN await them.
            // Awaiting inside the same loop would force serial execution
            // and let one slow controller (Tizen, HA-passthrough) block
            // every other display under the IPC 2-second budget.
            let handles: Vec<tokio::task::JoinHandle<(DisplayId, Result<(), CmdFailure>)>> =
                executors
                    .into_iter()
                    .map(|(display_id, sink)| {
                        tokio::spawn(async move { (display_id, sink.wake_once().await) })
                    })
                    .collect();

            let mut results: Vec<EmergencyWakeResult> = Vec::with_capacity(handles.len());
            for handle in handles {
                match handle.await {
                    Ok((display_id, Ok(()))) => results.push(EmergencyWakeResult {
                        display: display_id,
                        ok: true,
                        error: None,
                    }),
                    Ok((display_id, Err(failure))) => {
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
                        // A spawned per-display task panicked.  Match the
                        // CLI fallback's tolerance: log + continue, do
                        // not abort the report.  The row is dropped —
                        // the report still ships so the operator gets a
                        // structured view of the survivors.
                        tracing::warn!(
                            event = "emergency_wake_task_panicked",
                            error = %join_err,
                            "emergency-wake: spawned wake task panicked",
                        );
                    }
                }
            }

            let report = EmergencyWakeReport {
                paused: true,
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
    fn handle_exercise(&mut self, target: DisplayId, reply: oneshot::Sender<ExerciseReport>) {
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

        tokio::spawn(async move {
            let report = run_exercise_sequence(
                &sink,
                effective_mode,
                pre_phase,
                rules_to_resume.clone(),
                target,
            )
            .await;

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
            let _ = results_tx.send(InternalResult::ExerciseResume {
                rules: rules_to_resume,
            });

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
        let own_effects = self.feed_ownership(display, now);
        for effect in own_effects {
            self.process_effect(display, effect);
        }
        let Some(machine) = self.machines.get_mut(display) else {
            return;
        };
        let effects = machine.step(input, now);
        for effect in effects {
            self.process_effect(display, effect);
        }
    }

    /// Consult the [`OwnershipGate`] for `display` and, if the verdict
    /// differs from the last-fed value, feed
    /// [`Input::OwnershipChanged`] to the state machine.
    ///
    /// Returns any effects the machine produced (e.g. teardown-render on
    /// ownership-yield).  When the gate returns the same value as last
    /// time this is a no-op (returns `vec![]`).
    fn feed_ownership(&mut self, display: &DisplayId, now: Tick) -> Vec<Effect> {
        let owns = self.ownership.owns(display);
        if self.last_owned.get(display) == Some(&owns) {
            return vec![];
        }
        self.last_owned.insert(display.clone(), owns);
        let Some(machine) = self.machines.get_mut(display) else {
            return vec![];
        };
        machine.step(Input::OwnershipChanged(owns), now)
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
                displays.push((
                    dcfg.display.0.clone(),
                    DisplaySnapshot {
                        phase: m.phase_name().to_string(),
                        inhibited: m.overlays().inhibited,
                        paused: m.overlays().paused.is_some(),
                        cmd_gen: m.cmd_gen(),
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
                if let Some(machine) = self.machines.get_mut(&display) {
                    let effects = machine.step(Input::BlankResult { r#gen, result }, now);
                    for effect in effects {
                        self.process_effect(&display, effect);
                    }
                }
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
                if let Some(machine) = self.machines.get_mut(&display) {
                    let effects = machine.step(Input::WakeResult { r#gen, result }, now);
                    for effect in effects {
                        self.process_effect(&display, effect);
                    }
                }
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
                if let Some(machine) = self.machines.get_mut(&display) {
                    let effects = machine.step(Input::RenderResult { r#gen, result }, now);
                    for effect in effects {
                        self.process_effect(&display, effect);
                    }
                }
            }
            InternalResult::ExerciseResume { rules } => {
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
                        let own_effects = self.feed_ownership(&display, deadline);
                        for effect in own_effects {
                            self.process_effect(&display, effect);
                        }
                        if let Some(machine) = self.machines.get_mut(&display) {
                            let effects = machine.step(Input::Tick, deadline);
                            for effect in effects {
                                self.process_effect(&display, effect);
                            }
                        }
                    }
                    TimerEntry::DisplayStageTick(display, stage_gen) => {
                        let own_effects = self.feed_ownership(&display, deadline);
                        for effect in own_effects {
                            self.process_effect(&display, effect);
                        }
                        if let Some(machine) = self.machines.get_mut(&display) {
                            let effects =
                                machine.step(Input::StageTick { r#gen: stage_gen }, deadline);
                            for effect in effects {
                                self.process_effect(&display, effect);
                            }
                        }
                    }
                    TimerEntry::HoldExpiry(sensor_id) => {
                        self.fire_hold_expiry(&sensor_id, deadline);
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
    //    ORIGINAL baseline — the wake-path restoration check.
    let wake_result = sink.wake().await;
    let state_after_wake = bounded_read_state(sink.clone()).await;
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
        paused_rules,
        steps,
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
    use crate::traits::{PanelState, PowerState};

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
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"event\":\"wear_snapshot\""));
        assert!(matches!(
            serde_json::from_str(&s).unwrap(),
            DaemonEvent::WearSnapshot { .. }
        ));
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
            },
            ZoneEngine::new(vec![], &[]).expect("empty zone engine is valid"),
            HashMap::new(),
            HashMap::new(),
            Arc::new(crate::ownership::AlwaysOwned),
        )
        .expect("minimal engine config is valid")
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
        };
        engine.handle_control(ControlMsg::PublishDaemonEvent(ev));

        let got = sub.try_recv().expect("published event delivered");
        match got {
            DaemonEvent::WearSnapshot {
                display,
                total_on_hours,
                sample_count,
            } => {
                assert_eq!(display, DisplayId("m".into()));
                assert!((total_on_hours - 1.5).abs() < f64::EPSILON);
                assert_eq!(sample_count, 3);
            }
            other => panic!("expected WearSnapshot verbatim, got {other:?}"),
        }
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
        },
        zone_engine,
        machines,
        executors: HashMap::new(),
        render_sinks: HashMap::new(),
        ownership: Arc::new(AlwaysOwned),
        last_owned: HashMap::new(),
        rule_displays: HashMap::new(),
        zone_rules: HashMap::new(),
        paused_rules: HashSet::new(),
        holds: HashMap::new(),
        wake_attempts: HashMap::new(),
        reported: HashSet::new(),
        last_blank_failed: HashSet::new(),
        sensor_last_seen_virtual: HashMap::new(),
        timers: BinaryHeap::new(),
        results_rx,
        results_tx,
        event_tx,
        pending_reload: None,
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
        },
        zone_engine,
        machines,
        executors: HashMap::new(),
        render_sinks: HashMap::new(),
        ownership: Arc::new(NeverOwned),
        last_owned: HashMap::new(),
        rule_displays: HashMap::new(),
        zone_rules: HashMap::new(),
        paused_rules: HashSet::new(),
        holds: HashMap::new(),
        wake_attempts: HashMap::new(),
        reported: HashSet::new(),
        last_blank_failed: HashSet::new(),
        sensor_last_seen_virtual: HashMap::new(),
        timers: BinaryHeap::new(),
        results_rx,
        results_tx,
        event_tx,
        pending_reload: None,
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
