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

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::SensorKind;
use crate::error::DormantError;
use crate::state_machine::{DisplayStateMachine, Effect, Input, SmTimings};
use crate::traits::CommandSink;
use crate::types::{
    BlankMode, CmdFailure, DisplayId, PresenceEvent, RuleId, SensorId, SensorState, Tick,
    Timestamp, ZoneId,
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
}

/// Outbound events emitted by the engine for downstream consumers.
#[derive(Clone, Serialize, Debug)]
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
#[derive(Serialize, Debug, Clone)]
pub struct SensorSnapshot {
    /// The sensor id (as a string for JSON readability).
    pub id: String,
    /// Current sensor state.
    pub state: SensorState,
    /// Seconds since the last event arrived from this sensor.
    pub last_seen_secs_ago: u64,
}

/// A zone as seen by a [`StateSnapshot`].
#[derive(Serialize, Debug, Clone)]
pub struct ZoneSnapshot {
    /// The zone id.
    pub id: String,
    /// Resolved presence (`None` if the zone is unknown to the engine).
    pub present: Option<bool>,
}

/// A display as seen by a [`StateSnapshot`].
#[derive(Serialize, Debug, Clone)]
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
}

/// A point-in-time view of engine state, returned by [`ControlMsg::Snapshot`].
#[derive(Serialize, Debug, Clone)]
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
    /// The blank mode to issue on a successful blank.
    pub blank_mode: BlankMode,
    /// Timing parameters for the display's state machine.
    pub timings: SmTimings,
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
    results_rx: mpsc::Receiver<InternalResult>,
    /// Internal results mpsc — cloned into each spawned dispatch task.
    results_tx: mpsc::Sender<InternalResult>,
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
    /// Construct an engine from its runtime config, zone engine, and command
    /// executors.
    ///
    /// # Errors
    ///
    /// [`DormantError::ConfigInvalid`] if any rule references a display that
    /// has no [`DisplayRuntimeCfg`] and no [`CommandSink`] executor.
    pub fn new(
        cfg: RulesEngineConfig,
        zone_engine: ZoneEngine,
        executors: HashMap<DisplayId, Arc<dyn CommandSink>>,
    ) -> Result<Self, DormantError> {
        let now = Tick::now();

        // Build machines for every declared display.
        let mut machines: HashMap<DisplayId, DisplayStateMachine> = HashMap::new();
        for dcfg in &cfg.displays {
            machines.insert(
                dcfg.display.clone(),
                DisplayStateMachine::new(dcfg.timings.clone(), dcfg.blank_mode, now),
            );
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

        let (results_tx, results_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(256);

        Ok(Self {
            cfg,
            zone_engine,
            machines,
            executors,
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
    fn fan_zone_change_to_displays(&mut self, zone: &ZoneId, present: bool) {
        // Snapshot the rule ids so the immutable borrow on `self.zone_rules`
        // ends before we step machines mutably.
        let rule_ids: Vec<RuleId> = match self.zone_rules.get(zone) {
            Some(rs) => rs.clone(),
            None => return,
        };
        let now = Tick::now();
        for rule_id in rule_ids {
            if self.paused_rules.contains(&rule_id) {
                continue;
            }
            let displays: Vec<DisplayId> = match self.rule_displays.get(&rule_id) {
                Some(ds) => ds.clone(),
                None => continue,
            };
            for display_id in displays {
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
            ControlMsg::Snapshot(tx) => self.send_snapshot(tx),
            ControlMsg::SubscribeEvents(tx) => {
                let _ = tx.send(self.event_tx.subscribe());
            }
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

    /// Step a single display machine and process its effects.
    fn step_one(&mut self, display: &DisplayId, input: Input) {
        let Some(machine) = self.machines.get_mut(display) else {
            return;
        };
        let now = Tick::now();
        let effects = machine.step(input, now);
        for effect in effects {
            self.process_effect(display, effect);
        }
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
                displays.push((
                    dcfg.display.0.clone(),
                    DisplaySnapshot {
                        phase: m.phase_name().to_string(),
                        inhibited: m.overlays().inhibited,
                        paused: m.overlays().paused.is_some(),
                        cmd_gen: m.cmd_gen(),
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
                        if let Some(machine) = self.machines.get_mut(&display) {
                            let effects = machine.step(Input::Tick, deadline);
                            for effect in effects {
                                self.process_effect(&display, effect);
                            }
                        }
                    }
                    TimerEntry::HoldExpiry(sensor_id) => {
                        self.fire_hold_expiry(&sensor_id);
                    }
                }
            } else {
                break;
            }
        }
        let _ = now; // accepted parameter for symmetry
    }

    fn fire_hold_expiry(&mut self, sensor_id: &SensorId) {
        let pending = match self.holds.get_mut(sensor_id) {
            Some(h) => h.pending_absent.take(),
            None => None,
        };
        // Always disarm.
        if let Some(h) = self.holds.get_mut(sensor_id) {
            h.armed_until = None;
        }
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
                        let _ = tx
                            .send(InternalResult::Blank {
                                display,
                                r#gen,
                                result,
                            })
                            .await;
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
                        let _ = tx
                            .send(InternalResult::Wake {
                                display,
                                r#gen,
                                result,
                            })
                            .await;
                    });
                }
            }
            Effect::ScheduleTickAt(tick) => {
                self.timers
                    .push(Reverse((tick, TimerEntry::DisplayTick(display_id.clone()))));
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
