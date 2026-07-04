//! Per-display blank/wake state machine — the correctness-critical core of dormant.
//!
//! A [`DisplayStateMachine`] drives one display through its blank/wake lifecycle:
//! presence clears → grace countdown → blank command → presence returns → wake
//! command → active.  The machine is self-contained (no I/O) and driven by
//! [`Input`] events from the daemon event loop; it returns [`Effect`] values that
//! the caller executes.
//!
//! ## Invariants
//!
//! - Wake is never blocked by inhibitors, pauses, dwell timers, or holdoffs.
//!   A screen that won't wake is the worst failure mode.
//! - Command generations guard against stale results from timed-out or retried
//!   commands — a result with a non-matching generation is silently ignored.
//! - The status-quo interpretation always errs toward "present": an unknown zone
//!   level (never observed) is treated as present; only explicit `ZonePresent(false)`
//!   can trigger a blank.

use std::time::Duration;

use crate::types::{BlankMode, Tick};

// ── Phase ──────────────────────────────────────────────────────────────────────

/// The current state of a display in the blank/wake lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// Display is on and the zone reports presence.
    Active,
    /// Zone has cleared; counting down before issuing a blank command.
    Grace {
        /// Monotonic instant at which the grace period expires.
        until: Tick,
    },
    /// A blank command is in flight, awaiting its result.
    Blanking,
    /// Display is blanked (command succeeded).
    Blanked,
    /// A wake command is in flight, awaiting its result.
    Waking,
}

// ── Overlays ───────────────────────────────────────────────────────────────────

/// Pause state for manual or scheduled display suppression.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PauseState {
    /// When `Some`, auto-resume at this monotonic instant.
    /// When `None`, the pause is indefinite (until explicit `Resume`).
    pub until: Option<Tick>,
}

/// Overlay flags that gate or suppress automatic blank transitions.
///
/// Wake transitions are never gated by overlays — presence always wakes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlays {
    /// User-activity inhibitor is active (suppresses blanking while the user is
    /// at the keyboard).
    pub inhibited: bool,
    /// Manual or scheduled pause. `None` when not paused; `Some(PauseState)`
    /// when paused (indefinitely or with a deadline).
    pub paused: Option<PauseState>,
}

// ── Input ──────────────────────────────────────────────────────────────────────

/// An event fed into [`DisplayStateMachine::step`].
#[derive(Debug, Clone)]
pub enum Input {
    /// The presence sensor reports whether the zone is occupied.
    ZonePresent(bool),
    /// A monotonic tick — drives the grace countdown and wake-retry loop.
    Tick,
    /// The user-activity inhibitor state changed.
    InhibitorChanged(bool),
    /// Pause the display (blank path frozen; wake path unaffected).
    Pause {
        /// Auto-resume deadline, or `None` for indefinite.
        until: Option<Tick>,
    },
    /// Resume a previously paused display.
    Resume,
    /// Result of a previously issued blank command.
    BlankResult {
        /// Generation counter matching the `IssueBlank` that was sent.
        ///
        /// `r#gen` because `gen` is a reserved keyword in Rust 2024.
        r#gen: u64,
        /// `Ok(())` if the blank succeeded; `Err(CmdFailure)` on failure.
        result: Result<(), crate::types::CmdFailure>,
    },
    /// Result of a previously issued wake command.
    WakeResult {
        /// Generation counter matching the `IssueWake` that was sent.
        r#gen: u64,
        /// `Ok(())` if the wake succeeded; `Err(CmdFailure)` on failure.
        result: Result<(), crate::types::CmdFailure>,
    },
    /// Force-immediate blank (operator override — bypasses grace, inhibitors,
    /// pauses, holdoffs, and dwell timers).
    ForceBlank,
    /// Force-immediate wake (operator override).
    ForceWake,
}

// ── Effect ─────────────────────────────────────────────────────────────────────

/// An effect emitted by [`DisplayStateMachine::step`] for the caller to execute.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Issue a blank command to the display controller.
    IssueBlank {
        /// Monotonically increasing generation counter for stale-detection.
        r#gen: u64,
        /// The blank mode to use.
        mode: BlankMode,
    },
    /// Issue a wake command to the display controller.
    IssueWake {
        /// Monotonically increasing generation counter for stale-detection.
        r#gen: u64,
    },
    /// Schedule a [`Input::Tick`] at the given monotonic instant.
    ScheduleTickAt(Tick),
    /// Record a state transition for logging/metrics.
    LogTransition {
        /// Literal name of the source phase.
        from: &'static str,
        /// Literal name of the destination phase.
        to: &'static str,
        /// Literal cause string (grep-stable for alerting).
        cause: &'static str,
    },
}

// ── SmTimings ──────────────────────────────────────────────────────────────────

/// Timing parameters for a [`DisplayStateMachine`], built from rule + daemon
/// config by the caller.
///
/// `min_blank_time` is **reserved**: no M1 transition consumes it.
/// Wake-on-presence always wins; the field exists for M2 auto-cycling features.
#[derive(Debug, Clone)]
pub struct SmTimings {
    /// Debounce period before a blank is issued after the zone clears.
    pub grace_period: Duration,
    /// Reserved for M2 auto-cycling; not consumed by any M1 transition.
    pub min_blank_time: Duration,
    /// Minimum time a display must stay awake before it can be blanked again
    /// (anti-flap dwell on the blank path).
    pub min_wake_time: Duration,
    /// How long after daemon startup before the first blank is permitted
    /// (allows sensors to stabilise).
    pub startup_holdoff: Duration,
    /// Interval between wake retries when a wake command fails.
    pub wake_retry_interval: Duration,
}

// ── DisplayStateMachine ────────────────────────────────────────────────────────

/// Per-display state machine that drives blank/wake commands.
///
/// All fields are private — the public API is [`DisplayStateMachine::step`],
/// [`DisplayStateMachine::phase`], [`DisplayStateMachine::phase_name`],
/// [`DisplayStateMachine::overlays`], [`DisplayStateMachine::cmd_gen`], and
/// [`DisplayStateMachine::restore`].
pub struct DisplayStateMachine {
    phase: Phase,
    overlays: Overlays,
    /// Monotonically increasing command generation counter.
    cmd_gen: u64,
    /// Generation of the last issued blank command, for stale-detection.
    last_blank_gen: Option<u64>,
    /// Generation of the last issued wake command, for stale-detection.
    last_wake_gen: Option<u64>,
    /// When the last successful wake completed.
    last_wake: Option<Tick>,
    /// When the last successful blank completed.
    last_blank: Option<Tick>,
    /// Monotonic instant when this machine was created (for startup holdoff).
    started_at: Tick,
    /// The blank mode to use.
    blank_mode: BlankMode,
    /// Timing parameters.
    timings: SmTimings,
    /// Set when presence arrives during a blank command — the machine will
    /// transition to waking as soon as the blank result confirms.
    pending_wake: bool,
    /// Reserved for M2 auto-cycling; not consumed by any M1 transition.
    #[allow(dead_code)]
    pending_reblank: bool,
    /// Last observed zone level from `Input::ZonePresent`. `None` until the
    /// first `ZonePresent` input arrives.
    zone_present: Option<bool>,
    /// When `Some`, the grace countdown is frozen with this much time
    /// remaining.  Set when inhibitor or pause activates during Grace; cleared
    /// when both inhibitor and pause are removed.
    grace_frozen_remaining: Option<Duration>,
}

impl DisplayStateMachine {
    /// Create a new state machine starting in [`Phase::Active`].
    #[must_use]
    pub fn new(timings: SmTimings, blank_mode: BlankMode, now: Tick) -> Self {
        Self {
            phase: Phase::Active,
            overlays: Overlays::default(),
            cmd_gen: 0,
            last_blank_gen: None,
            last_wake_gen: None,
            last_wake: None,
            last_blank: None,
            started_at: now,
            blank_mode,
            timings,
            pending_wake: false,
            pending_reblank: false,
            zone_present: None,
            grace_frozen_remaining: None,
        }
    }

    /// Restore a state machine from a snapshot (e.g. after a daemon reload).
    ///
    /// `cmd_gen` carries over to avoid generation reuse.  Runtime-only state
    /// (overlays, pending flags, zone level, frozen grace) is reset.
    ///
    /// Returns `(Self, Vec<Effect>)` with any initial scheduling effects the
    /// restored phase requires: a `ScheduleTickAt` for Waking or Grace, so
    /// the machine owns its own exit driver from the moment it is restored.
    #[must_use]
    pub fn restore(
        timings: SmTimings,
        blank_mode: BlankMode,
        phase: Phase,
        cmd_gen: u64,
        now: Tick,
    ) -> (Self, Vec<Effect>) {
        let mut sm = Self {
            phase: Phase::Active, // placeholder, overwritten below
            overlays: Overlays::default(),
            cmd_gen,
            last_blank_gen: None,
            last_wake_gen: None,
            last_wake: None,
            last_blank: None,
            started_at: now,
            blank_mode,
            timings,
            pending_wake: false,
            pending_reblank: false,
            zone_present: None,
            grace_frozen_remaining: None,
        };

        let effects = match phase {
            Phase::Waking => {
                sm.phase = Phase::Waking;
                vec![Effect::ScheduleTickAt(Tick(
                    now.0 + sm.timings.wake_retry_interval,
                ))]
            }
            Phase::Grace { until } => {
                sm.phase = Phase::Grace { until };
                // Overlays are reset — not frozen — so schedule the tick.
                vec![Effect::ScheduleTickAt(until)]
            }
            other => {
                sm.phase = other;
                vec![]
            }
        };

        (sm, effects)
    }

    /// Feed an input event and return zero or more effects to execute.
    ///
    /// The match is exhaustive over all (phase, input) pairs — no catch-all arm
    /// so that every combination is explicitly reasoned about.
    #[must_use]
    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    pub fn step(&mut self, input: Input, now: Tick) -> Vec<Effect> {
        let result = match (&self.phase, input) {
            // ── Active ──────────────────────────────────────────────────────
            // Zone becomes absent → start grace countdown (pre-frozen if
            // inhibitor or pause is already active).
            (Phase::Active, Input::ZonePresent(present)) => {
                self.zone_present = Some(present);
                if present {
                    vec![]
                } else {
                    self.enter_grace(now, "zone_clear")
                }
            }
            // Tick in Active: only serves pause auto-resume.
            (Phase::Active, Input::Tick) => self.maybe_auto_resume(now),
            // Inhibitor changes are recorded; they gate blank at Grace expiry.
            (Phase::Active, Input::InhibitorChanged(inhibited)) => {
                self.overlays.inhibited = inhibited;
                vec![]
            }
            // Pause records the overlay and optionally schedules auto-resume.
            (Phase::Active, Input::Pause { until }) => {
                self.overlays.paused = Some(PauseState { until });
                if let Some(deadline) = until {
                    vec![Effect::ScheduleTickAt(deadline)]
                } else {
                    vec![]
                }
            }
            // Resume clears the pause overlay.
            (Phase::Active, Input::Resume) => {
                self.overlays.paused = None;
                vec![]
            }
            // Stale blank result — no blank was issued from Active.
            (Phase::Active, Input::BlankResult { .. }) => {
                vec![]
            }
            // Stale wake result — no wake was issued from Active.
            (Phase::Active, Input::WakeResult { .. }) => {
                vec![]
            }
            // ForceBlank bypasses grace, inhibitors, and dwell.
            (Phase::Active, Input::ForceBlank) => self.issue_blank(vec![Effect::LogTransition {
                from: "active",
                to: "blanking",
                cause: "force_blank",
            }]),
            // ForceWake in Active is a no-op — already awake.
            (Phase::Active, Input::ForceWake) => {
                vec![]
            }

            // ── Grace ───────────────────────────────────────────────────────
            // Presence returns during grace → cancel blank, go active.
            (Phase::Grace { .. }, Input::ZonePresent(present)) => {
                self.zone_present = Some(present);
                if present {
                    self.enter_active(now, "presence_during_grace")
                } else {
                    // Re-assertion of absence; no change.
                    vec![]
                }
            }
            // Grace tick — check expiry and blanket gates.
            (Phase::Grace { until }, Input::Tick) => {
                let until = *until;

                // Pause auto-resume fires first.  If it unfreezes the
                // countdown, the recomputed until + schedule is
                // authoritative — return immediately so expiry-eval runs
                // on its own tick, never against the stale `until`.
                let was_frozen = self.grace_frozen_remaining.is_some();
                let mut effects = self.maybe_auto_resume(now);
                if was_frozen && self.grace_frozen_remaining.is_none() {
                    // Auto-resume just unfroze the grace — new until has
                    // been computed and scheduled.  Return; the
                    // rescheduled tick will evaluate expiry correctly.
                    return effects;
                }

                // If the countdown is frozen, the tick is ignored.
                if self.grace_frozen_remaining.is_some() {
                    return effects;
                }

                // Belt-and-braces: if an overlay arrived after Grace entry
                // (e.g. restore path), freeze the countdown now.
                if self.overlays.inhibited || self.overlays.paused.is_some() {
                    self.freeze_grace(until.0, now.0);
                    return effects;
                }

                // Not yet expired.
                if now < until {
                    return effects;
                }

                // Check blank gates: startup holdoff, min-wake dwell.
                // (inhibitor/pause already handled above — they trigger
                // freeze, not a simple gate check.)

                let mut blocked = false;
                let mut reschedule_at = now.0;

                // Startup holdoff: gate the first blank only.
                let elapsed = now.0.duration_since(self.started_at.0);
                if elapsed < self.timings.startup_holdoff {
                    blocked = true;
                    let candidate = self.started_at.0 + self.timings.startup_holdoff;
                    if candidate > reschedule_at {
                        reschedule_at = candidate;
                    }
                }

                // Min-wake dwell: prevent rapid blank→wake→blank cycling.
                if let Some(lw) = self.last_wake {
                    let since_wake = now.0.duration_since(lw.0);
                    if since_wake < self.timings.min_wake_time {
                        blocked = true;
                        let candidate = lw.0 + self.timings.min_wake_time;
                        if candidate > reschedule_at {
                            reschedule_at = candidate;
                        }
                    }
                }

                if blocked {
                    effects.push(Effect::ScheduleTickAt(Tick(reschedule_at)));
                    return effects;
                }

                // All gates passed — issue blank.
                effects.push(Effect::LogTransition {
                    from: "grace",
                    to: "blanking",
                    cause: "grace_expired",
                });
                effects.append(&mut self.issue_blank(vec![]));
                effects
            }
            // Inhibitor activation during Grace: freeze the countdown.
            (Phase::Grace { until }, Input::InhibitorChanged(inhibited)) => {
                self.overlays.inhibited = inhibited;
                if inhibited {
                    self.freeze_grace(until.0, now.0);
                    vec![]
                } else {
                    self.unfreeze_grace(now)
                }
            }
            // Pause during Grace: freeze the countdown like inhibitor.
            (Phase::Grace { until }, Input::Pause { until: pause_until }) => {
                let was_paused = self.overlays.paused.is_some();
                self.overlays.paused = Some(PauseState { until: pause_until });
                let mut effects = Vec::new();
                if !was_paused {
                    self.freeze_grace(until.0, now.0);
                }
                if let Some(deadline) = pause_until {
                    effects.push(Effect::ScheduleTickAt(deadline));
                }
                effects
            }
            // Resume during Grace: unfreeze if all freeze sources are gone.
            (Phase::Grace { .. }, Input::Resume) => {
                self.overlays.paused = None;
                self.unfreeze_grace(now)
            }
            // Stale blank result — ignored.
            (Phase::Grace { .. }, Input::BlankResult { .. }) => {
                vec![]
            }
            // Stale wake result — ignored.
            (Phase::Grace { .. }, Input::WakeResult { .. }) => {
                vec![]
            }
            // ForceBlank bypasses grace, inhibitors, and dwell.
            (Phase::Grace { .. }, Input::ForceBlank) => {
                self.grace_frozen_remaining = None;
                self.issue_blank(vec![Effect::LogTransition {
                    from: "grace",
                    to: "blanking",
                    cause: "force_blank",
                }])
            }
            // ForceWake during Grace: abort countdown, go active.
            // Routes through enter_active so an absent zone immediately
            // re-enters Grace (display gets grace_period of screen time,
            // then normal rules resume).
            (Phase::Grace { .. }, Input::ForceWake) => {
                self.grace_frozen_remaining = None;
                self.enter_active(now, "force_wake")
            }

            // ── Blanking ────────────────────────────────────────────────────
            // Presence during blanking: defer wake until blank result arrives.
            (Phase::Blanking, Input::ZonePresent(present)) => {
                self.zone_present = Some(present);
                if present {
                    self.pending_wake = true;
                }
                vec![]
            }
            // Tick in Blanking: waiting for BlankResult; no action.
            (Phase::Blanking, Input::Tick) => self.maybe_auto_resume(now),
            // Inhibitor in Blanking: record; does not affect in-flight command.
            (Phase::Blanking, Input::InhibitorChanged(inhibited)) => {
                self.overlays.inhibited = inhibited;
                vec![]
            }
            // Pause in Blanking: record; does not affect in-flight command.
            (Phase::Blanking, Input::Pause { until }) => {
                self.overlays.paused = Some(PauseState { until });
                if let Some(deadline) = until {
                    vec![Effect::ScheduleTickAt(deadline)]
                } else {
                    vec![]
                }
            }
            // Resume in Blanking.
            (Phase::Blanking, Input::Resume) => {
                self.overlays.paused = None;
                vec![]
            }
            // BlankResult with matching generation: process.
            (Phase::Blanking, Input::BlankResult { r#gen, result }) => {
                if Some(r#gen) != self.last_blank_gen {
                    // Stale generation — ignore.
                    return vec![];
                }
                if result.is_ok() {
                    self.last_blank = Some(now);
                    if self.pending_wake {
                        self.pending_wake = false;
                        self.issue_wake(
                            vec![Effect::LogTransition {
                                from: "blanking",
                                to: "waking",
                                cause: "presence_during_blank",
                            }],
                            now,
                        )
                    } else {
                        self.phase = Phase::Blanked;
                        vec![Effect::LogTransition {
                            from: "blanking",
                            to: "blanked",
                            cause: "blank_succeeded",
                        }]
                    }
                } else {
                    // Blank failed.
                    self.pending_wake = false;
                    if self.zone_present == Some(false) {
                        // Zone is still absent — re-enter Grace directly so
                        // the retry chain has a driver.  No external edge
                        // required.
                        self.enter_grace(now, "blank_failed_regrace")
                    } else {
                        // Presence returned in the meantime — go Active.
                        self.enter_active(now, "blank_failed")
                    }
                }
            }
            // Stale wake result — ignored.
            (Phase::Blanking, Input::WakeResult { .. }) => {
                vec![]
            }
            // ForceBlank in Blanking: already blanking; no-op.
            (Phase::Blanking, Input::ForceBlank) => {
                vec![]
            }
            // ForceWake in Blanking: defer to after blank result.
            (Phase::Blanking, Input::ForceWake) => {
                self.pending_wake = true;
                vec![]
            }

            // ── Blanked ─────────────────────────────────────────────────────
            // Presence returns → wake immediately.
            (Phase::Blanked, Input::ZonePresent(present)) => {
                self.zone_present = Some(present);
                if present {
                    self.issue_wake(
                        vec![Effect::LogTransition {
                            from: "blanked",
                            to: "waking",
                            cause: "presence_detected",
                        }],
                        now,
                    )
                } else {
                    vec![]
                }
            }
            // Tick in Blanked: no action.
            (Phase::Blanked, Input::Tick) => self.maybe_auto_resume(now),
            // Inhibitor in Blanked: record; wake path is unaffected.
            (Phase::Blanked, Input::InhibitorChanged(inhibited)) => {
                self.overlays.inhibited = inhibited;
                vec![]
            }
            // Pause in Blanked: record; wake path is unaffected.
            (Phase::Blanked, Input::Pause { until }) => {
                self.overlays.paused = Some(PauseState { until });
                if let Some(deadline) = until {
                    vec![Effect::ScheduleTickAt(deadline)]
                } else {
                    vec![]
                }
            }
            // Resume in Blanked.
            (Phase::Blanked, Input::Resume) => {
                self.overlays.paused = None;
                vec![]
            }
            // Stale blank result — ignored.
            (Phase::Blanked, Input::BlankResult { .. }) => {
                vec![]
            }
            // Stale wake result — ignored.
            (Phase::Blanked, Input::WakeResult { .. }) => {
                vec![]
            }
            // ForceBlank in Blanked: already blanked; no-op.
            (Phase::Blanked, Input::ForceBlank) => {
                vec![]
            }
            // ForceWake in Blanked: wake immediately.
            (Phase::Blanked, Input::ForceWake) => self.issue_wake(
                vec![Effect::LogTransition {
                    from: "blanked",
                    to: "waking",
                    cause: "force_wake",
                }],
                now,
            ),

            // ── Waking ──────────────────────────────────────────────────────
            // Zone level is recorded but wake completes first.
            (Phase::Waking, Input::ZonePresent(present)) => {
                self.zone_present = Some(present);
                vec![]
            }
            // Tick in Waking: re-issue wake (liveness — never gated).
            (Phase::Waking, Input::Tick) => {
                let mut effects = self.maybe_auto_resume(now);
                effects.push(Effect::LogTransition {
                    from: "waking",
                    to: "waking",
                    cause: "wake_retry_scheduled",
                });
                effects.append(&mut self.issue_wake(vec![], now));
                // issue_wake already schedules the next retry tick — no
                // extra ScheduleTickAt needed here.
                effects
            }
            // Inhibitor in Waking: recorded but does not gate wake.
            (Phase::Waking, Input::InhibitorChanged(inhibited)) => {
                self.overlays.inhibited = inhibited;
                vec![]
            }
            // Pause in Waking: recorded but does not gate wake.
            (Phase::Waking, Input::Pause { until }) => {
                self.overlays.paused = Some(PauseState { until });
                if let Some(deadline) = until {
                    vec![Effect::ScheduleTickAt(deadline)]
                } else {
                    vec![]
                }
            }
            // Resume in Waking.
            (Phase::Waking, Input::Resume) => {
                self.overlays.paused = None;
                vec![]
            }
            // Stale blank result — ignored.
            (Phase::Waking, Input::BlankResult { .. }) => {
                vec![]
            }
            // WakeResult with matching generation: process.
            (Phase::Waking, Input::WakeResult { r#gen, result }) => {
                if Some(r#gen) != self.last_wake_gen {
                    // Stale generation — ignore.
                    return vec![];
                }
                if result.is_ok() {
                    self.last_wake = Some(now);
                    self.enter_active(now, "wake_completed")
                } else {
                    // Wake failed: immediately re-issue with fresh gen (the
                    // executor's own burst already backed off) and schedule
                    // the next retry tick.
                    let mut effects = vec![Effect::LogTransition {
                        from: "waking",
                        to: "waking",
                        cause: "wake_retry",
                    }];
                    effects.append(&mut self.issue_wake(vec![], now));
                    effects
                }
            }
            // ForceBlank in Waking: cancel the wake loop, blank immediately.
            (Phase::Waking, Input::ForceBlank) => self.issue_blank(vec![Effect::LogTransition {
                from: "waking",
                to: "blanking",
                cause: "force_blank",
            }]),
            // ForceWake in Waking: restart the wake attempt with a fresh
            // generation.
            (Phase::Waking, Input::ForceWake) => self.issue_wake(vec![], now),
        };

        // Invariant: frozen-Grace state is never observable from a
        // non-Grace phase (Must 2).
        debug_assert!(
            matches!(self.phase, Phase::Grace { .. }) || self.grace_frozen_remaining.is_none(),
            "grace_frozen_remaining ({:?}) must be None outside Grace phase ({:?})",
            self.grace_frozen_remaining,
            self.phase_name()
        );

        result
    }

    /// Return the current phase.
    #[must_use]
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Return the literal name of the current phase, grep-stable for logging.
    #[must_use]
    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Active => "active",
            Phase::Grace { .. } => "grace",
            Phase::Blanking => "blanking",
            Phase::Blanked => "blanked",
            Phase::Waking => "waking",
        }
    }

    /// Return the current overlay flags.
    #[must_use]
    pub fn overlays(&self) -> &Overlays {
        &self.overlays
    }

    /// Return the current command generation counter (for snapshot carry-over).
    #[must_use]
    pub fn cmd_gen(&self) -> u64 {
        self.cmd_gen
    }

    // ── Private helpers ─────────────────────────────────────────────────────

    /// Freeze the grace countdown, capturing the remaining time.
    fn freeze_grace(&mut self, until: std::time::Instant, now: std::time::Instant) {
        if self.grace_frozen_remaining.is_none() {
            // `Instant - Instant` saturates at zero.
            self.grace_frozen_remaining = Some(until - now);
        }
    }

    /// Unfreeze the grace countdown if no freeze sources (inhibitor or pause)
    /// remain active.  Resets `until` to `now + remaining` and schedules a tick.
    fn unfreeze_grace(&mut self, now: Tick) -> Vec<Effect> {
        if self.grace_frozen_remaining.is_none() {
            return vec![];
        }
        if self.overlays.inhibited || self.overlays.paused.is_some() {
            return vec![];
        }
        let remaining = self.grace_frozen_remaining.take().unwrap_or_default();
        let until = Tick(now.0 + remaining);
        self.phase = Phase::Grace { until };
        vec![Effect::ScheduleTickAt(until)]
    }

    /// If paused with a deadline that has passed, auto-resume.
    fn maybe_auto_resume(&mut self, now: Tick) -> Vec<Effect> {
        if let Some(ref ps) = self.overlays.paused
            && let Some(deadline) = ps.until
            && now >= deadline
        {
            self.overlays.paused = None;
            // If in Grace with frozen countdown, unfreeze.
            return self.unfreeze_grace(now);
        }
        vec![]
    }

    /// Enter Active, then immediately chain into Grace if zone is known absent.
    /// All transitions to Active MUST route through this helper so the deferred-
    /// zone-clear chain is never missed.
    fn enter_active(&mut self, now: Tick, cause: &'static str) -> Vec<Effect> {
        // Clear any stale frozen-Grace state (Must 2: frozen state must be
        // impossible to observe from any non-Grace phase).
        self.grace_frozen_remaining = None;
        let from = self.phase_name();
        self.phase = Phase::Active;
        let mut effects = vec![Effect::LogTransition {
            from,
            to: "active",
            cause,
        }];
        // If zone is known absent, immediately begin grace.
        if self.zone_present == Some(false) {
            effects.append(&mut self.enter_grace(now, "deferred_zone_clear"));
        }
        effects
    }

    /// Enter Grace, pre-freezing if any overlay is already active.
    /// Emits `ScheduleTickAt` only when the countdown is live (not frozen).
    fn enter_grace(&mut self, now: Tick, cause: &'static str) -> Vec<Effect> {
        let from = self.phase_name();
        let mut effects = vec![Effect::LogTransition {
            from,
            to: "grace",
            cause,
        }];

        let frozen = self.overlays.inhibited || self.overlays.paused.is_some();
        if frozen {
            // Pre-freeze: capture the full grace period as remaining.
            // The countdown will unfreeze when all overlays clear.
            self.grace_frozen_remaining = Some(self.timings.grace_period);
            // Sentinel `until` — frozen gates ignore Tick so its value is
            // harmless; the real `until` is set on unfreeze.
            let sentinel = Tick(now.0 + self.timings.grace_period);
            self.phase = Phase::Grace { until: sentinel };
        } else {
            // Live countdown — defensive clear of any stale frozen state
            // from a prior Grace period.
            self.grace_frozen_remaining = None;
            let until = Tick(now.0 + self.timings.grace_period);
            self.phase = Phase::Grace { until };
            effects.push(Effect::ScheduleTickAt(until));
        }
        effects
    }

    /// Bump the command generation and emit an `IssueBlank` effect, setting the
    /// phase to `Blanking`.  `prefix` effects (e.g. `LogTransition`) are placed
    /// before the `IssueBlank`.
    fn issue_blank(&mut self, prefix: Vec<Effect>) -> Vec<Effect> {
        self.cmd_gen = self.cmd_gen.wrapping_add(1);
        let r#gen = self.cmd_gen;
        self.last_blank_gen = Some(r#gen);
        self.phase = Phase::Blanking;
        let mut effects = prefix;
        effects.push(Effect::IssueBlank {
            r#gen,
            mode: self.blank_mode,
        });
        effects
    }

    /// Bump the command generation and emit an `IssueWake` effect, setting the
    /// phase to `Waking`.  Every entry into Waking also schedules the retry
    /// driver — the machine owns its own exit.
    /// `prefix` effects (e.g. `LogTransition`) are placed before the
    /// `IssueWake`; the `ScheduleTickAt` tail comes last.
    fn issue_wake(&mut self, prefix: Vec<Effect>, now: Tick) -> Vec<Effect> {
        self.cmd_gen = self.cmd_gen.wrapping_add(1);
        let r#gen = self.cmd_gen;
        self.last_wake_gen = Some(r#gen);
        self.phase = Phase::Waking;
        let mut effects = prefix;
        effects.push(Effect::IssueWake { r#gen });
        effects.push(Effect::ScheduleTickAt(Tick(
            now.0 + self.timings.wake_retry_interval,
        )));
        effects
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;

    // ── Helpers ─────────────────────────────────────────────────────────────

    /// Create a `Tick` at `offset_ms` milliseconds from now.
    fn t(offset_ms: u64) -> Tick {
        Tick(
            std::time::Instant::now()
                .checked_add(Duration::from_millis(offset_ms))
                .unwrap(),
        )
    }

    /// Create `SmTimings` with the given `grace_period`, using zero-duration
    /// defaults for gates that should not block by default.
    fn timings(grace_ms: u64) -> SmTimings {
        SmTimings {
            grace_period: Duration::from_millis(grace_ms),
            min_blank_time: Duration::from_secs(10),
            min_wake_time: Duration::from_secs(0),
            startup_holdoff: Duration::from_secs(0),
            wake_retry_interval: Duration::from_millis(100),
        }
    }

    /// Create a fresh state machine with the given grace period.
    fn sm(grace_ms: u64) -> DisplayStateMachine {
        DisplayStateMachine::new(timings(grace_ms), BlankMode::PowerOff, t(0))
    }

    /// Assert that `effects` contains exactly one `IssueBlank` with the
    /// expected generation.
    fn assert_issue_blank(effects: &[Effect], expected_gen: u64) {
        let blanks: Vec<_> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::IssueBlank { r#gen, .. } = e {
                    Some(*r#gen)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            blanks.len(),
            1,
            "expected exactly 1 IssueBlank, got {}: {effects:?}",
            blanks.len()
        );
        assert_eq!(blanks[0], expected_gen);
    }

    /// Assert that `effects` contains exactly one `IssueWake` with the
    /// expected generation.
    fn assert_issue_wake(effects: &[Effect], expected_gen: u64) {
        let wakes: Vec<_> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::IssueWake { r#gen } = e {
                    Some(*r#gen)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            wakes.len(),
            1,
            "expected exactly 1 IssueWake, got {}: {effects:?}",
            wakes.len()
        );
        assert_eq!(wakes[0], expected_gen);
    }

    /// Assert that `effects` contains a `ScheduleTickAt` and return its tick.
    fn get_schedule_tick(effects: &[Effect]) -> Tick {
        for e in effects {
            if let Effect::ScheduleTickAt(tick) = e {
                return *tick;
            }
        }
        panic!("no ScheduleTickAt in effects");
    }

    /// Drive a blank-result round-trip: issue blank, feed result.
    fn drive_blank(sm: &mut DisplayStateMachine, now: Tick, ok: bool) {
        let r#gen = sm.cmd_gen() + 1;
        let effects = sm.step(Input::ForceBlank, now);
        assert_issue_blank(&effects, r#gen);
        let result = if ok {
            Ok(())
        } else {
            Err(crate::types::CmdFailure {
                controller: "test".into(),
                error: "failure".into(),
            })
        };
        let _ = sm.step(Input::BlankResult { r#gen, result }, now);
    }

    /// Drive a wake-result round-trip: issue wake, feed result.
    #[allow(dead_code)]
    fn drive_wake(sm: &mut DisplayStateMachine, now: Tick, ok: bool) {
        let r#gen = sm.cmd_gen() + 1;
        let effects = sm.step(Input::ForceWake, now);
        assert_issue_wake(&effects, r#gen);
        let result = if ok {
            Ok(())
        } else {
            Err(crate::types::CmdFailure {
                controller: "test".into(),
                error: "failure".into(),
            })
        };
        let _ = sm.step(Input::WakeResult { r#gen, result }, now);
    }

    // ── Happy path ──────────────────────────────────────────────────────────

    #[test]
    fn grace_then_blank_happy_path() {
        let t0 = t(0);
        let mut sm = DisplayStateMachine::new(timings(500), BlankMode::PowerOff, t0);

        // Zone clears → Grace.
        let effects = sm.step(Input::ZonePresent(false), t0);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        let grace_tick = get_schedule_tick(&effects);
        assert!(grace_tick > t0);

        // Tick at grace expiry → Blanking + IssueBlank.
        let effects = sm.step(Input::Tick, grace_tick);
        assert!(
            matches!(sm.phase(), Phase::Blanking),
            "expected Blanking, got {:?}",
            sm.phase()
        );
        assert_issue_blank(&effects, 1);

        // BlankResult Ok → Blanked.
        let _effects = sm.step(
            Input::BlankResult {
                r#gen: 1,
                result: Ok(()),
            },
            grace_tick,
        );
        assert!(matches!(sm.phase(), Phase::Blanked));
        assert_eq!(sm.phase_name(), "blanked");
    }

    #[test]
    fn presence_during_grace_cancels_blank() {
        let mut sm = sm(500);
        sm.step(Input::ZonePresent(false), t(0));
        assert!(matches!(sm.phase(), Phase::Grace { .. }));

        // Presence returns → Active.
        let effects = sm.step(Input::ZonePresent(true), t(100));
        assert!(matches!(sm.phase(), Phase::Active));

        // LogTransition documents the cause.
        let has_cause = effects.iter().any(|e| {
            matches!(
                e,
                Effect::LogTransition {
                    cause: "presence_during_grace",
                    ..
                }
            )
        });
        assert!(has_cause, "expected presence_during_grace log transition");
    }

    #[test]
    fn wake_is_never_blocked_by_inhibitor_or_pause() {
        let mut sm = sm(500);
        let t0 = t(0);

        // Blank the display.
        drive_blank(&mut sm, t0, true);
        assert!(matches!(sm.phase(), Phase::Blanked));

        // Activate inhibitor and pause.
        sm.step(Input::InhibitorChanged(true), t(100));
        sm.step(
            Input::Pause {
                until: Some(t(5000)),
            },
            t(100),
        );

        // Presence still wakes.
        let effects = sm.step(Input::ZonePresent(true), t(100));
        assert!(matches!(sm.phase(), Phase::Waking));
        assert_issue_wake(&effects, sm.cmd_gen());
    }

    #[test]
    fn min_wake_dwell_defers_reblank() {
        let mut sm = DisplayStateMachine::new(
            SmTimings {
                grace_period: Duration::from_millis(100),
                min_blank_time: Duration::from_secs(10),
                min_wake_time: Duration::from_millis(500),
                startup_holdoff: Duration::from_secs(0),
                wake_retry_interval: Duration::from_secs(60),
            },
            BlankMode::PowerOff,
            t(0),
        );

        // Blank, then wake.
        let t_blank = t(0);
        drive_blank(&mut sm, t_blank, true);
        assert!(matches!(sm.phase(), Phase::Blanked));
        let t_wake = t(50);
        let effects = sm.step(Input::ZonePresent(true), t_wake);
        assert_issue_wake(&effects, sm.cmd_gen());
        sm.step(
            Input::WakeResult {
                r#gen: sm.cmd_gen(),
                result: Ok(()),
            },
            t_wake,
        );
        assert!(matches!(sm.phase(), Phase::Active));

        // Immediately zone clears — grace starts.
        let t_clear = t(100);
        let effects = sm.step(Input::ZonePresent(false), t_clear);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        let grace_tick = get_schedule_tick(&effects);

        // Tick at grace expiry — min_wake_dwell blocks (only 50ms since wake,
        // but min_wake_time is 500ms).
        let effects = sm.step(Input::Tick, grace_tick);
        // Should still be in Grace (blocked), with a reschedule.
        assert!(
            matches!(sm.phase(), Phase::Grace { .. }),
            "expected Grace, got {:?}",
            sm.phase()
        );
        let reschedule = get_schedule_tick(&effects);
        assert!(reschedule > grace_tick);

        // Later tick past the dwell → blank proceeds.
        let effects = sm.step(Input::Tick, reschedule);
        assert!(matches!(sm.phase(), Phase::Blanking));
        assert_issue_blank(&effects, sm.cmd_gen());
    }

    #[test]
    fn startup_holdoff_blocks_first_blank_only() {
        let mut sm = DisplayStateMachine::new(
            SmTimings {
                grace_period: Duration::from_millis(100),
                min_blank_time: Duration::from_secs(10),
                min_wake_time: Duration::from_secs(0),
                startup_holdoff: Duration::from_secs(1),
                wake_retry_interval: Duration::from_secs(60),
            },
            BlankMode::PowerOff,
            t(0),
        );

        // Zone clears immediately.
        let effects = sm.step(Input::ZonePresent(false), t(0));
        let grace_tick = get_schedule_tick(&effects);

        // Tick at grace expiry — startup holdoff blocks.
        let effects = sm.step(Input::Tick, grace_tick);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        let reschedule = get_schedule_tick(&effects);

        // Tick past holdoff — blank proceeds.
        let effects = sm.step(Input::Tick, reschedule);
        assert!(matches!(sm.phase(), Phase::Blanking));
        let blank_gen = sm.cmd_gen();
        assert_issue_blank(&effects, blank_gen);

        // Complete blank → wake → zone clears again.
        sm.step(
            Input::BlankResult {
                r#gen: blank_gen,
                result: Ok(()),
            },
            reschedule,
        );
        let effects = sm.step(Input::ZonePresent(true), reschedule);
        let wake_gen = sm.cmd_gen();
        assert_issue_wake(&effects, wake_gen);
        sm.step(
            Input::WakeResult {
                r#gen: wake_gen,
                result: Ok(()),
            },
            reschedule,
        );
        assert!(matches!(sm.phase(), Phase::Active));

        // Second zone clear → grace → tick → blank proceeds immediately
        // (startup holdoff no longer applies).
        let t2 = t(2000);
        let effects = sm.step(Input::ZonePresent(false), t2);
        let grace_tick2 = get_schedule_tick(&effects);
        let _effects = sm.step(Input::Tick, grace_tick2);
        assert!(
            matches!(sm.phase(), Phase::Blanking),
            "second blank should not be blocked by startup holdoff"
        );
    }

    #[test]
    fn pause_freezes_grace_but_not_waking() {
        let mut sm = sm(500);
        let t0 = t(0);

        // Enter Grace.
        sm.step(Input::ZonePresent(false), t0);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));

        // Pause — freezes countdown.
        sm.step(Input::Pause { until: None }, t(100));
        assert!(sm.overlays().paused.is_some());

        // Tick past grace — ignored (frozen).
        let effects = sm.step(Input::Tick, t(600));
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        assert!(effects.is_empty());

        // But wake path is unaffected: ForceWake in Grace goes to Active,
        // then chains into Grace because zone is still absent (Should).
        let _effects = sm.step(Input::ForceWake, t(600));
        assert!(
            matches!(sm.phase(), Phase::Grace { .. }),
            "ForceWake in Grace with absent zone should re-chain to Grace"
        );
    }

    #[test]
    fn pause_with_deadline_auto_resumes() {
        let t0 = t(0);
        let deadline = t(800);
        let mid = t(500);
        let mut sm = sm(500);

        // Pause with deadline.
        let effects = sm.step(
            Input::Pause {
                until: Some(deadline),
            },
            t0,
        );
        assert!(sm.overlays().paused.is_some());
        // Should have scheduled a tick at the deadline.
        let scheduled = get_schedule_tick(&effects);
        assert_eq!(scheduled, deadline);

        // Tick before deadline — still paused.
        let _effects = sm.step(Input::Tick, mid);
        assert!(sm.overlays().paused.is_some());

        // Tick at deadline — auto-resume.
        let _effects = sm.step(Input::Tick, deadline);
        assert!(sm.overlays().paused.is_none());
    }

    #[test]
    fn force_blank_bypasses_grace_but_presence_still_wakes() {
        let mut sm = sm(500);
        sm.step(Input::ZonePresent(false), t(0));
        assert!(matches!(sm.phase(), Phase::Grace { .. }));

        // ForceBlank → Blanking immediately.
        let effects = sm.step(Input::ForceBlank, t(100));
        assert!(matches!(sm.phase(), Phase::Blanking));
        let blank_gen = sm.cmd_gen();
        assert_issue_blank(&effects, blank_gen);

        // Presence arrives during blanking → deferred wake.
        sm.step(Input::ZonePresent(true), t(150));
        assert!(matches!(sm.phase(), Phase::Blanking));

        // Blank completes → wakes immediately.
        let _effects = sm.step(
            Input::BlankResult {
                r#gen: blank_gen,
                result: Ok(()),
            },
            t(200),
        );
        assert!(matches!(sm.phase(), Phase::Waking));
    }

    #[test]
    fn failed_wake_reissues_on_tick_without_presence_edge() {
        let mut sm = sm(500);
        let t0 = t(0);

        // Drive to Blanked.
        drive_blank(&mut sm, t0, true);
        assert!(matches!(sm.phase(), Phase::Blanked));

        // Presence wakes.
        let effects = sm.step(Input::ZonePresent(true), t(100));
        assert_issue_wake(&effects, 2); // gen1 was blank, gen2 is wake
        let wake_gen = 2;

        // Wake fails — immediately re-issues gen3 (Must 2).
        let effects = sm.step(
            Input::WakeResult {
                r#gen: wake_gen,
                result: Err(crate::types::CmdFailure {
                    controller: "test".into(),
                    error: "fail".into(),
                }),
            },
            t(150),
        );
        assert!(matches!(sm.phase(), Phase::Waking));
        assert_issue_wake(&effects, 3); // immediate re-issue
        let retry_at = get_schedule_tick(&effects);

        // Tick fires — re-issues wake with gen4.
        let effects = sm.step(Input::Tick, retry_at);
        assert!(matches!(sm.phase(), Phase::Waking));
        assert_issue_wake(&effects, 4);
    }

    #[test]
    fn stale_generation_result_is_ignored() {
        let mut sm = sm(500);
        let t0 = t(0);

        // Drive to Blanked.
        drive_blank(&mut sm, t0, true);
        assert!(matches!(sm.phase(), Phase::Blanked));

        // Issue wake gen=2.
        sm.step(Input::ZonePresent(true), t(100));
        assert!(matches!(sm.phase(), Phase::Waking));

        // Deliver WakeResult with gen=1 (the old blank gen) — should be ignored.
        let effects = sm.step(
            Input::WakeResult {
                r#gen: 1,
                result: Ok(()),
            },
            t(150),
        );
        assert!(
            matches!(sm.phase(), Phase::Waking),
            "stale gen should not change phase"
        );
        assert!(effects.is_empty(), "stale gen should produce no effects");
    }

    #[test]
    fn presence_during_blanking_wakes_after_blank_result() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Start blanking.
        let effects = sm.step(Input::ZonePresent(false), t0);
        let grace_tick = get_schedule_tick(&effects);
        let _effects = sm.step(Input::Tick, grace_tick); // → Blanking
        assert!(matches!(sm.phase(), Phase::Blanking));

        // Presence arrives during blanking.
        let mid = Tick(grace_tick.0 + Duration::from_millis(50));
        let _effects = sm.step(Input::ZonePresent(true), mid);
        assert!(matches!(sm.phase(), Phase::Blanking));

        // BlankResult Ok → transitions to Waking (not Blanked).
        let later = Tick(mid.0 + Duration::from_millis(50));
        let effects = sm.step(
            Input::BlankResult {
                r#gen: 1,
                result: Ok(()),
            },
            later,
        );
        assert!(
            matches!(sm.phase(), Phase::Waking),
            "expected Waking, got {:?}",
            sm.phase()
        );
        // Verify the transition log shows the pending_wake path.
        let has_waking = effects.iter().any(|e| {
            matches!(
                e,
                Effect::LogTransition {
                    to: "waking",
                    cause: "presence_during_blank",
                    ..
                }
            )
        });
        assert!(has_waking, "expected presence_during_blank transition");
    }

    #[test]
    fn zone_clear_during_waking_completes_wake_first() {
        let mut sm = sm(500);
        let t0 = t(0);

        // Drive to Blanked, then wake.
        drive_blank(&mut sm, t0, true);
        sm.step(Input::ZonePresent(true), t(100));
        assert!(matches!(sm.phase(), Phase::Waking));
        let wake_gen = sm.cmd_gen();

        // Zone clears during waking — recorded but does not interrupt.
        sm.step(Input::ZonePresent(false), t(150));
        assert!(matches!(sm.phase(), Phase::Waking));

        // Wake completes → Active → immediately Grace (deferred_zone_clear).
        let effects = sm.step(
            Input::WakeResult {
                r#gen: wake_gen,
                result: Ok(()),
            },
            t(200),
        );
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        let has_deferred = effects.iter().any(|e| {
            matches!(
                e,
                Effect::LogTransition {
                    cause: "deferred_zone_clear",
                    ..
                }
            )
        });
        assert!(has_deferred, "expected deferred_zone_clear transition");
    }

    #[test]
    fn inhibitor_freezes_and_unfreezes_grace() {
        let mut sm = sm(500);
        let t0 = t(0);

        // Enter Grace.
        sm.step(Input::ZonePresent(false), t0);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));

        // Inhibitor activates — freezes countdown.
        sm.step(Input::InhibitorChanged(true), t(200));
        assert!(sm.overlays().inhibited);

        // Tick past original expiry — ignored.
        let effects = sm.step(Input::Tick, t(600));
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        assert!(effects.is_empty());

        // Inhibitor deactivates — unfreezes with remaining time, schedules tick.
        let effects = sm.step(Input::InhibitorChanged(false), t(800));
        let reschedule = get_schedule_tick(&effects);
        // Remaining was ~300ms (500 - 200), so reschedule ≈ t(1100).
        assert!(reschedule > t(800));
    }

    #[test]
    fn blank_result_err_returns_to_active() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Start blanking.
        let effects = sm.step(Input::ZonePresent(false), t0);
        let grace_tick = get_schedule_tick(&effects);
        let _effects = sm.step(Input::Tick, grace_tick); // → Blanking
        assert!(matches!(sm.phase(), Phase::Blanking));
        let blank_gen = 1;

        // Blank fails — zone is absent, so re-enters Grace directly (Must 3).
        let later = Tick(grace_tick.0 + Duration::from_millis(100));
        let effects = sm.step(
            Input::BlankResult {
                r#gen: blank_gen,
                result: Err(crate::types::CmdFailure {
                    controller: "test".into(),
                    error: "fail".into(),
                }),
            },
            later,
        );
        assert!(
            matches!(sm.phase(), Phase::Grace { .. }),
            "expected Grace (zone absent), got {:?}",
            sm.phase()
        );
        let has_regrace = effects.iter().any(|e| {
            matches!(
                e,
                Effect::LogTransition {
                    cause: "blank_failed_regrace",
                    ..
                }
            )
        });
        assert!(has_regrace, "expected blank_failed_regrace transition");
    }

    /// Must 3 alternate: blank fails while presence returned → Active.
    #[test]
    fn blank_result_err_goes_active_when_present() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Start blanking, but presence returns.
        let effects = sm.step(Input::ZonePresent(false), t0);
        let grace_tick = get_schedule_tick(&effects);
        let _effects = sm.step(Input::Tick, grace_tick); // → Blanking
        sm.step(
            Input::ZonePresent(true),
            Tick(grace_tick.0 + Duration::from_millis(50)),
        );

        let blank_gen = 1;
        let later = Tick(grace_tick.0 + Duration::from_millis(100));
        let effects = sm.step(
            Input::BlankResult {
                r#gen: blank_gen,
                result: Err(crate::types::CmdFailure {
                    controller: "test".into(),
                    error: "fail".into(),
                }),
            },
            later,
        );
        assert!(
            matches!(sm.phase(), Phase::Active),
            "expected Active (presence returned), got {:?}",
            sm.phase()
        );
        let has_bf = effects.iter().any(|e| {
            matches!(
                e,
                Effect::LogTransition {
                    cause: "blank_failed",
                    ..
                }
            )
        });
        assert!(has_bf, "expected blank_failed transition");
    }

    #[test]
    fn phase_name_literal_values_pinned() {
        let s = sm(500);
        assert_eq!(s.phase_name(), "active");

        let mut s = sm(500);
        s.step(Input::ZonePresent(false), t(0));
        assert_eq!(s.phase_name(), "grace");

        let mut s = sm(500);
        s.step(Input::ForceBlank, t(0));
        assert_eq!(s.phase_name(), "blanking");

        // Drive to Blanked.
        let mut s = sm(500);
        drive_blank(&mut s, t(0), true);
        assert_eq!(s.phase_name(), "blanked");

        // Drive to Waking.
        let mut s = sm(500);
        drive_blank(&mut s, t(0), true);
        s.step(Input::ZonePresent(true), t(100));
        assert_eq!(s.phase_name(), "waking");
    }

    #[test]
    fn restore_carries_over_cmd_gen_and_phase() {
        let (sm, _effects) = DisplayStateMachine::restore(
            timings(500),
            BlankMode::ScreenOffAudioOn,
            Phase::Blanked,
            42,
            t(0),
        );
        assert_eq!(sm.cmd_gen(), 42);
        assert_eq!(sm.phase_name(), "blanked");
        // Runtime state is reset.
        assert!(!sm.overlays().inhibited);
        assert!(sm.overlays().paused.is_none());
    }

    #[test]
    fn restore_into_waking_emits_schedule_tick() {
        let (_sm, effects) =
            DisplayStateMachine::restore(timings(500), BlankMode::PowerOff, Phase::Waking, 7, t(0));
        // Restoring into Waking must own its exit driver.
        let has_schedule = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(has_schedule, "restore into Waking must emit ScheduleTickAt");
    }

    #[test]
    fn restore_into_grace_emits_schedule_tick() {
        let (_sm, effects) = DisplayStateMachine::restore(
            timings(500),
            BlankMode::PowerOff,
            Phase::Grace { until: t(500) },
            0,
            t(0),
        );
        // Restoring into Grace (not frozen — overlays are reset) must own its
        // exit driver.
        let has_schedule = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(has_schedule, "restore into Grace must emit ScheduleTickAt");
    }

    // ── Must-breaking-sequence unit tests ─────────────────────────────────

    /// Must 1: every entry into Waking schedules the retry driver.
    /// Breaking sequence: `ForceBlank` → `ZonePresent(true)` during `Blanking` →
    /// `BlankResult{Ok}` → `Waking` via `pending_wake`. `IssueWake` MUST be
    /// accompanied by `ScheduleTickAt`.
    #[test]
    fn must1_waking_entry_schedules_retry_driver() {
        let t0 = t(0);
        let mut sm = sm(500);

        // ForceBlank from Active.
        let effects = sm.step(Input::ForceBlank, t0);
        let blank_gen = sm.cmd_gen();
        assert_issue_blank(&effects, blank_gen);

        // Presence arrives during blanking.
        let t1 = Tick(t0.0 + Duration::from_millis(50));
        sm.step(Input::ZonePresent(true), t1);

        // BlankResult Ok → Waking via pending_wake.
        let t2 = Tick(t0.0 + Duration::from_millis(100));
        let effects = sm.step(
            Input::BlankResult {
                r#gen: blank_gen,
                result: Ok(()),
            },
            t2,
        );
        assert!(matches!(sm.phase(), Phase::Waking));
        // The effects batch MUST contain both IssueWake and ScheduleTickAt.
        let has_wake = effects
            .iter()
            .any(|e| matches!(e, Effect::IssueWake { .. }));
        let has_tick = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(has_wake, "Must 1: Waking entry must emit IssueWake");
        assert!(
            has_tick,
            "Must 1: Waking entry must own its exit driver via ScheduleTickAt"
        );
    }

    /// Must 2: `WakeResult(Err)` immediately re-issues `IssueWake` + `ScheduleTickAt`.
    #[test]
    fn must2_wake_fail_immediately_reissues() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Drive to Blanked, then wake.
        drive_blank(&mut sm, t0, true);
        let _effects = sm.step(Input::ZonePresent(true), t(100));
        let wake_gen = sm.cmd_gen();

        // Wake fails → must emit IssueWake with fresh gen + ScheduleTickAt.
        let effects = sm.step(
            Input::WakeResult {
                r#gen: wake_gen,
                result: Err(crate::types::CmdFailure {
                    controller: "test".into(),
                    error: "fail".into(),
                }),
            },
            t(150),
        );
        assert!(matches!(sm.phase(), Phase::Waking));
        let has_wake = effects
            .iter()
            .any(|e| matches!(e, Effect::IssueWake { .. }));
        let has_tick = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(
            has_wake,
            "Must 2: WakeResult(Err) must re-issue IssueWake immediately"
        );
        assert!(
            has_tick,
            "Must 2: WakeResult(Err) must schedule retry tick immediately"
        );
    }

    /// Must 3: BlankResult(Err) with absent zone re-enters Grace directly.
    #[test]
    fn must3_blank_fail_absent_zone_regraces() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Enter Blanking.
        let effects = sm.step(Input::ZonePresent(false), t0);
        let grace_tick = get_schedule_tick(&effects);
        sm.step(Input::Tick, grace_tick);
        assert!(matches!(sm.phase(), Phase::Blanking));

        // Blank fails, zone is absent → must enter Grace with ScheduleTickAt.
        let later = Tick(grace_tick.0 + Duration::from_millis(100));
        let effects = sm.step(
            Input::BlankResult {
                r#gen: 1,
                result: Err(crate::types::CmdFailure {
                    controller: "test".into(),
                    error: "fail".into(),
                }),
            },
            later,
        );
        assert!(
            matches!(sm.phase(), Phase::Grace { .. }),
            "Must 3: blank fail + absent zone must regrace"
        );
        let has_tick = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(
            has_tick,
            "Must 3: blank-fail regrace must own its exit driver via ScheduleTickAt"
        );
    }

    /// Must 4: inhibitor active BEFORE Grace entry → pre-frozen countdown.
    #[test]
    fn must4_inhibitor_before_grace_prefreezes() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Activate inhibitor first.
        sm.step(Input::InhibitorChanged(true), t0);
        assert!(sm.overlays().inhibited);

        // Zone clears → Grace should be pre-frozen (no ScheduleTickAt).
        let effects = sm.step(Input::ZonePresent(false), t(100));
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        let has_tick = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(
            !has_tick,
            "Must 4: pre-frozen Grace must NOT emit ScheduleTickAt"
        );

        // Tick past grace — ignored (frozen).
        let effects = sm.step(Input::Tick, t(700));
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        assert!(effects.is_empty());

        // Inhibitor deactivates — unfreezes with full grace period.
        let effects = sm.step(Input::InhibitorChanged(false), t(800));
        let has_tick = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(
            has_tick,
            "Must 4: uninhibitor must schedule the unfrozen grace tick"
        );
    }

    /// Must 1 (round 2): pause auto-resume must not use the stale deadline.
    /// Seq: Grace until t500 → Pause{until:t800}@t100 freezes (remaining 400)
    /// → Tick@t800 unfreezes → must NOT issue blank on the stale t500
    /// deadline; blank must fire at ~t1200 on the rescheduled tick.
    #[test]
    fn pause_auto_resume_does_not_use_stale_deadline() {
        let t0 = t(0);
        let mut sm = DisplayStateMachine::new(
            SmTimings {
                grace_period: Duration::from_millis(500),
                min_blank_time: Duration::from_secs(10),
                min_wake_time: Duration::from_secs(0),
                startup_holdoff: Duration::from_secs(0),
                wake_retry_interval: Duration::from_millis(100),
            },
            BlankMode::PowerOff,
            t0,
        );

        // Enter Grace at t0 (until=t500).
        let _effects = sm.step(Input::ZonePresent(false), t0);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));

        // Pause at t100 with deadline t800 → freezes countdown.
        let t_pause = Tick(t0.0 + Duration::from_millis(100));
        sm.step(
            Input::Pause {
                until: Some(Tick(t0.0 + Duration::from_millis(800))),
            },
            t_pause,
        );

        // Tick at t800 — auto-resume unfreezes. Must NOT issue blank.
        let t800 = Tick(t0.0 + Duration::from_millis(800));
        let effects = sm.step(Input::Tick, t800);

        // No IssueBlank (stale deadline check).
        let has_blank = effects
            .iter()
            .any(|e| matches!(e, Effect::IssueBlank { .. }));
        assert!(
            !has_blank,
            "Must 1: stale deadline must not trigger IssueBlank"
        );

        // Phase should still be Grace (unfrozen with new until).
        assert!(
            matches!(sm.phase(), Phase::Grace { .. }),
            "Must 1: should still be in Grace after unfreeze"
        );

        // The rescheduled tick (~t1200 = t800 + 400 remaining) should fire
        // and produce a blank.
        let reschedule = get_schedule_tick(&effects);
        let effects = sm.step(Input::Tick, reschedule);
        let has_blank = effects
            .iter()
            .any(|e| matches!(e, Effect::IssueBlank { .. }));
        assert!(has_blank, "Must 1: blank must fire on the rescheduled tick");
    }

    /// Must 2 (round 2): frozen-Grace state cleared on presence exit.
    /// Seq: Grace → freeze → ZonePresent(true) → Active (frozen leaked)
    /// → inhibitor clears → later ZonePresent(false) → second Grace must
    /// blank normally at its expiry (not frozen-ignored).
    #[test]
    fn frozen_grace_state_cleared_on_presence_exit() {
        let t0 = t(0);
        let mut sm = DisplayStateMachine::new(
            SmTimings {
                grace_period: Duration::from_millis(300),
                min_blank_time: Duration::from_secs(10),
                min_wake_time: Duration::from_secs(0),
                startup_holdoff: Duration::from_secs(0),
                wake_retry_interval: Duration::from_millis(100),
            },
            BlankMode::PowerOff,
            t0,
        );

        // Enter Grace, then freeze via inhibitor.
        let _effects = sm.step(Input::ZonePresent(false), t0);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));
        let t1 = Tick(t0.0 + Duration::from_millis(50));
        sm.step(Input::InhibitorChanged(true), t1);

        // Presence returns — exits Grace.  frozen_remaining must be cleared.
        let t2 = Tick(t0.0 + Duration::from_millis(100));
        sm.step(Input::ZonePresent(true), t2);
        assert!(matches!(sm.phase(), Phase::Active));

        // Inhibitor clears (no-op for Grace since we're in Active).
        let t3 = Tick(t0.0 + Duration::from_millis(150));
        sm.step(Input::InhibitorChanged(false), t3);

        // Zone clears again — second Grace must be live, not frozen.
        let t4 = Tick(t0.0 + Duration::from_millis(200));
        let effects = sm.step(Input::ZonePresent(false), t4);
        let grace_tick2 = get_schedule_tick(&effects);

        // Tick at expiry — must blank (not frozen-ignored).
        let _effects = sm.step(Input::Tick, grace_tick2);
        assert!(
            matches!(sm.phase(), Phase::Blanking),
            "Must 2: second Grace must blank at expiry, got {:?}",
            sm.phase()
        );
    }

    /// Should (concurred): `ForceWake` in Grace re-chains when zone absent.
    #[test]
    fn force_wake_in_grace_rechains_when_zone_absent() {
        let t0 = t(0);
        let mut sm = sm(500);

        // Zone absent → Grace.
        sm.step(Input::ZonePresent(false), t0);
        assert!(matches!(sm.phase(), Phase::Grace { .. }));

        // ForceWake → Active → immediately Grace (zone still absent).
        let effects = sm.step(Input::ForceWake, t(200));
        assert!(
            matches!(sm.phase(), Phase::Grace { .. }),
            "ForceWake in Grace with absent zone should re-chain to Grace"
        );
        let has_tick = effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
        assert!(
            has_tick,
            "re-chained Grace must own its exit driver via ScheduleTickAt"
        );
    }

    // ── Proptest: liveness & safety ─────────────────────────────────────────

    mod proptest_helpers {
        use super::*;
        use proptest::prelude::*;

        /// Generate an arbitrary `Input` given the current `cmd_gen` for
        /// plausible result generations.
        pub fn arb_input(_cmd_gen: u64) -> impl Strategy<Value = Input> {
            // Wide range so stale and future gens both occur.
            let gen_range = (0u64..=4u64).boxed();

            prop_oneof![
                // ZonePresent
                any::<bool>().prop_map(Input::ZonePresent),
                // Tick
                Just(Input::Tick),
                // InhibitorChanged
                any::<bool>().prop_map(Input::InhibitorChanged),
                // Pause
                (any::<bool>(), any::<u64>()).prop_map(|(has_deadline, offset)| {
                    if has_deadline {
                        Input::Pause {
                            until: Some(Tick(
                                std::time::Instant::now() + Duration::from_millis(offset),
                            )),
                        }
                    } else {
                        Input::Pause { until: None }
                    }
                }),
                // Resume
                Just(Input::Resume),
                // BlankResult with plausible gen
                (gen_range.clone(), any::<bool>()).prop_map(|(r#gen, ok)| {
                    Input::BlankResult {
                        r#gen,
                        result: if ok {
                            Ok(())
                        } else {
                            Err(crate::types::CmdFailure {
                                controller: "prop".into(),
                                error: "simulated".into(),
                            })
                        },
                    }
                }),
                // WakeResult with plausible gen
                (gen_range, any::<bool>()).prop_map(|(r#gen, ok)| {
                    Input::WakeResult {
                        r#gen,
                        result: if ok {
                            Ok(())
                        } else {
                            Err(crate::types::CmdFailure {
                                controller: "prop".into(),
                                error: "simulated".into(),
                            })
                        },
                    }
                }),
                // ForceBlank
                Just(Input::ForceBlank),
                // ForceWake
                Just(Input::ForceWake),
            ]
        }

        /// A recorded issued command for safety analysis.
        pub enum IssuedGen {
            /// A blank was issued.
            Blank,
            /// A wake was issued.
            Wake,
        }

        /// Track effects emitted during a proptest run.
        pub fn track_issued(effects: &[Effect], issued: &mut Vec<IssuedGen>) {
            for e in effects {
                match e {
                    Effect::IssueBlank { .. } => issued.push(IssuedGen::Blank),
                    Effect::IssueWake { .. } => issued.push(IssuedGen::Wake),
                    _ => {}
                }
            }
        }

        /// Drive the state machine to Active using deterministic inputs.
        /// Returns the `now` Tick after recovery.
        pub fn drive_to_recovery(sm: &mut DisplayStateMachine, mut now: Tick) -> Tick {
            // Resolve any in-flight command left over from random steps.
            if matches!(sm.phase(), Phase::Blanking) {
                let r#gen = sm.cmd_gen();
                let _ = sm.step(
                    Input::BlankResult {
                        r#gen,
                        result: Ok(()),
                    },
                    now,
                );
            }
            if matches!(sm.phase(), Phase::Waking) {
                let r#gen = sm.cmd_gen();
                let _ = sm.step(
                    Input::WakeResult {
                        r#gen,
                        result: Ok(()),
                    },
                    now,
                );
            }

            // Feed presence.
            let _ = sm.step(Input::ZonePresent(true), now);

            for _round in 0..20 {
                let effects = sm.step(Input::Tick, now);

                // Process scheduled ticks first.
                let mut schedules: Vec<Tick> = effects
                    .iter()
                    .filter_map(|e| {
                        if let Effect::ScheduleTickAt(tick) = e {
                            Some(*tick)
                        } else {
                            None
                        }
                    })
                    .collect();

                // Process issued commands.
                for e in &effects {
                    match e {
                        Effect::IssueBlank { r#gen, .. } => {
                            let _ = sm.step(
                                Input::BlankResult {
                                    r#gen: *r#gen,
                                    result: Ok(()),
                                },
                                now,
                            );
                        }
                        Effect::IssueWake { r#gen } => {
                            let _ = sm.step(
                                Input::WakeResult {
                                    r#gen: *r#gen,
                                    result: Ok(()),
                                },
                                now,
                            );
                        }
                        _ => {}
                    }
                }

                // Feed scheduled ticks.
                for sched in schedules.drain(..) {
                    now = sched;
                    let sub = sm.step(Input::Tick, sched);
                    // Process any commands issued by the tick.
                    for e in &sub {
                        match e {
                            Effect::IssueBlank { r#gen, .. } => {
                                let _ = sm.step(
                                    Input::BlankResult {
                                        r#gen: *r#gen,
                                        result: Ok(()),
                                    },
                                    sched,
                                );
                            }
                            Effect::IssueWake { r#gen } => {
                                let _ = sm.step(
                                    Input::WakeResult {
                                        r#gen: *r#gen,
                                        result: Ok(()),
                                    },
                                    sched,
                                );
                            }
                            _ => {}
                        }
                    }
                }

                if matches!(sm.phase(), Phase::Active) {
                    break;
                }
                // Advance time slightly for the next round.
                now = Tick(now.0 + Duration::from_millis(1));
            }
            now
        }
    }

    use proptest::prop_assert;
    use proptest::proptest;
    use proptest_helpers::*;

    proptest! {
        #[test]
        fn state_machine_liveness_and_safety(
            steps in proptest::collection::vec(
                (arb_input(0), 1u64..600u64),
                1..200,
            ),
        ) {
            let mut sm = sm(500);
            let mut issued: Vec<IssuedGen> = Vec::new();

            // Replay random steps.
            for (input, offset_ms) in &steps {
                let now = Tick(
                    std::time::Instant::now()
                        .checked_add(Duration::from_millis(*offset_ms))
                        .unwrap(),
                );

                // Step must never panic.
                let effects = sm.step(input.clone(), now);
                track_issued(&effects, &mut issued);

                // Property: every effects batch containing IssueWake MUST also
                // contain ScheduleTickAt (the machine owns its exit driver).
                let has_wake = effects.iter().any(|e| matches!(e, Effect::IssueWake { .. }));
                if has_wake {
                    let has_tick = effects
                        .iter()
                        .any(|e| matches!(e, Effect::ScheduleTickAt(_)));
                    prop_assert!(
                        has_tick,
                        "IssueWake batch missing ScheduleTickAt at step {}",
                        offset_ms
                    );
                }
            }

            // LIVENESS: after feeding presence + driving deterministically,
            // the machine MUST recover to Active.
            let base = std::time::Instant::now()
                .checked_add(Duration::from_secs(1))
                .unwrap();
            let recovery_now = drive_to_recovery(&mut sm, Tick(base));
            prop_assert!(
                matches!(sm.phase(), Phase::Active),
                "machine stuck in {:?} after recovery drive; now={recovery_now:?}",
                sm.phase()
            );
        }

        #[test]
        fn lost_wake_result_recovers_via_ticks(
            steps in proptest::collection::vec(
                (arb_input(0), 1u64..600u64),
                1..200,
            ),
        ) {
            let mut sm = sm(500);

            // Replay random steps.
            for (input, offset_ms) in &steps {
                let now = Tick(
                    std::time::Instant::now()
                        .checked_add(Duration::from_millis(*offset_ms))
                        .unwrap(),
                );
                let _ = sm.step(input.clone(), now);
            }

            // Resolve any in-flight command left over from random steps, so
            // we start the tick-only drive in a clean state.
            if matches!(sm.phase(), Phase::Blanking) {
                let r#gen = sm.cmd_gen();
                let _ = sm.step(
                    Input::BlankResult {
                        r#gen,
                        result: Ok(()),
                    },
                    Tick(std::time::Instant::now()),
                );
            }
            if matches!(sm.phase(), Phase::Waking) {
                let r#gen = sm.cmd_gen();
                let _ = sm.step(
                    Input::WakeResult {
                        r#gen,
                        result: Ok(()),
                    },
                    Tick(std::time::Instant::now()),
                );
            }

            // Feed presence to try to wake.
            let base = std::time::Instant::now()
                .checked_add(Duration::from_secs(1))
                .unwrap();
            let mut now = Tick(base);
            let _ = sm.step(Input::ZonePresent(true), now);

            // Drive only with Ticks at each ScheduleTickAt for up to 10
            // rounds — never deliver WakeResults.  The machine must keep
            // emitting IssueWake (liveness under lost results).
            for _round in 0..10 {
                let effects = sm.step(Input::Tick, now);

                // Collect scheduled ticks.
                let schedules: Vec<Tick> = effects
                    .iter()
                    .filter_map(|e| {
                        if let Effect::ScheduleTickAt(t) = e {
                            Some(*t)
                        } else {
                            None
                        }
                    })
                    .collect();

                // If the machine is Waking, ticks MUST produce IssueWake.
                if matches!(sm.phase(), Phase::Waking) {
                    let has_wake = effects.iter().any(|e| matches!(e, Effect::IssueWake { .. }));
                    prop_assert!(
                        has_wake,
                        "Waking + Tick must emit IssueWake (lost-result liveness)"
                    );
                }

                if schedules.is_empty() {
                    break;
                }
                now = schedules[0];
            }

            // Now deliver ONE WakeResult with the current gen → must reach Active.
            if matches!(sm.phase(), Phase::Waking) {
                let r#gen = sm.cmd_gen();
                let _ = sm.step(
                    Input::WakeResult {
                        r#gen,
                        result: Ok(()),
                    },
                    now,
                );
            }
            prop_assert!(
                matches!(sm.phase(), Phase::Active),
                "machine stuck in {:?} after WakeResult Ok",
                sm.phase()
            );
        }
    }
}
