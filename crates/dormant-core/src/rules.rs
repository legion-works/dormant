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
use crate::traits::{CommandSink, RenderSink};
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
            ControlMsg::Snapshot(tx) => self.send_snapshot(tx),
            ControlMsg::SubscribeEvents(tx) => {
                let _ = tx.send(self.event_tx.subscribe());
            }
            ControlMsg::SetInhibited { rule, inhibited } => {
                self.handle_set_inhibited(rule.as_ref(), inhibited);
            }
            ControlMsg::SetPendingReload(detail) => self.set_pending_reload(detail),
            ControlMsg::EmergencyWake { reply } => self.handle_emergency_wake(reply),
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
    /// via the existing pause fan-out, then spawn a task that walks every
    /// display's executor and calls [`CommandSink::wake_once`] in parallel
    /// (one task per display), aggregating results into a
    /// [`EmergencyWakeReport`] the handler sends back through the
    /// oneshot.
    ///
    /// The wake execution runs **outside** the engine run loop so a slow
    /// network wake (Tizen, HA-passthrough) does not stall other control
    /// messages.  The IPC server wraps this call in its own 2-second
    /// timeout (see `dormantd::ipc::handle_emergency_wake`); the
    /// `dormantctl` client falls back to direct-hardware construction when
    /// that window elapses.
    fn handle_emergency_wake(&mut self, reply: oneshot::Sender<EmergencyWakeReport>) {
        // Pause every rule indefinitely — reuse the existing pause path so
        // the state-machine overlays route the same way as a normal global
        // pause.  Without this an absent-zone would re-trigger blanking on
        // the very next sensor event.
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
            "emergency-wake: paused all rules, dispatching one wake_once per display",
        );

        tokio::spawn(async move {
            let mut results: Vec<EmergencyWakeResult> = Vec::with_capacity(executors.len());
            for (display_id, sink) in executors {
                match sink.wake_once().await {
                    Ok(()) => results.push(EmergencyWakeResult {
                        display: display_id,
                        ok: true,
                        error: None,
                    }),
                    Err(failure) => {
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
                }
            }
            let report = EmergencyWakeReport {
                paused: true,
                displays: results,
            };
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
                if let Some(machine) = self.machines.get_mut(&display) {
                    let effects = machine.step(Input::BlankResult { r#gen, result }, now);
                    for effect in effects {
                        self.process_effect(&display, effect);
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
                    self.wake_attempts.remove(&display);
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
                    // TODO(Task 8): this engine-level path is covered at the
                    // SM level (render_only_total_cascade_returns_active) but
                    // not engine-level until a real/fake RenderSink is wired
                    // into rules_end_to_end.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
