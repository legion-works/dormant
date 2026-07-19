//! Deterministic timing policy for the audio inhibitor poller.

use dormant_core::rules::InhibitorKind;
use std::time::{Duration, Instant};

const FAILURE_THRESHOLD: u32 = 2;

/// The classified or operational outcome of one audio probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The `pw-dump` output was classified successfully.
    Classified {
        /// A playback stream is active.
        playback: bool,
        /// A call stream is active.
        call: bool,
    },
    /// Spawning, waiting for, or parsing the probe failed.
    Failure,
    /// The unreapable-child list reached its capacity.
    ReapCapReached,
}

impl ProbeOutcome {
    #[cfg(test)]
    const fn classified(playback: bool, call: bool) -> Self {
        Self::Classified { playback, call }
    }
}

/// A desired update from the audio timing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioTransition {
    /// Assert one audio inhibitor kind.
    Assert(InhibitorKind),
    /// Deassert one audio inhibitor kind.
    Deassert(InhibitorKind),
    /// Stop spawning probes until the cooldown expires.
    OpenBreaker,
    /// Resume probing after the unreapable-child list drains.
    CloseBreaker,
    /// Preserve the prior externally visible state.
    NoChange,
}

#[derive(Debug, Default, Clone, Copy)]
struct KindDebounce {
    active_since: Option<Instant>,
    effective: bool,
}

/// Pure timing and failure policy for the audio inhibitor.
///
/// The caller supplies the clock for every probe outcome. This state owns no
/// process, channel, or timer; it only decides which control edges the poller
/// should publish.
#[derive(Debug)]
pub(crate) struct AudioPolicy {
    min_active: Duration,
    breaker_cooldown: Duration,
    playback: KindDebounce,
    call: KindDebounce,
    consecutive_failures: u32,
    startup_grace: bool,
    breaker_open: bool,
    cooldown_until: Option<Instant>,
}

impl AudioPolicy {
    /// Create a fresh policy for one poller generation.
    #[must_use]
    pub(crate) fn new(min_active: Duration, breaker_cooldown: Duration) -> Self {
        Self {
            min_active,
            breaker_cooldown,
            playback: KindDebounce::default(),
            call: KindDebounce::default(),
            consecutive_failures: 0,
            startup_grace: true,
            breaker_open: false,
            cooldown_until: None,
        }
    }

    /// Fold one probe outcome into the policy using the caller's clock.
    #[must_use]
    pub(crate) fn step(&mut self, now: Instant, outcome: ProbeOutcome) -> Vec<AudioTransition> {
        let mut transitions = Vec::with_capacity(3);
        if self.breaker_open && !matches!(outcome, ProbeOutcome::ReapCapReached) {
            self.breaker_open = false;
            self.cooldown_until = None;
            transitions.push(AudioTransition::CloseBreaker);
        }

        match outcome {
            ProbeOutcome::Classified { playback, call } => {
                self.consecutive_failures = 0;
                transitions.push(Self::update_kind(
                    now,
                    playback,
                    &mut self.playback,
                    InhibitorKind::AudioPlayback,
                    self.startup_grace,
                    self.min_active,
                ));
                transitions.push(Self::update_kind(
                    now,
                    call,
                    &mut self.call,
                    InhibitorKind::Call,
                    self.startup_grace,
                    self.min_active,
                ));
                self.startup_grace = false;
            }
            ProbeOutcome::Failure => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= FAILURE_THRESHOLD {
                    self.reset_debounce();
                    transitions.extend(Self::deassert_all());
                }
            }
            ProbeOutcome::ReapCapReached => {
                self.cooldown_until = Some(now + self.breaker_cooldown);
                transitions.extend(Self::deassert_all());
                self.reset_debounce();
                if !self.breaker_open {
                    self.breaker_open = true;
                    transitions.push(AudioTransition::OpenBreaker);
                }
            }
        }

        if transitions.is_empty() {
            transitions.push(AudioTransition::NoChange);
        }
        transitions
    }

    /// Whether the reap-cap circuit breaker remains open.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn breaker_is_open(&self) -> bool {
        self.breaker_open
    }

    /// Remaining cooldown before another reap-cap check may retry spawning.
    #[must_use]
    pub(crate) fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn update_kind(
        now: Instant,
        active: bool,
        debounce: &mut KindDebounce,
        kind: InhibitorKind,
        startup_grace: bool,
        min_active: Duration,
    ) -> AudioTransition {
        if active {
            if debounce.active_since.is_none() {
                debounce.active_since = Some(now);
            }
            debounce.effective = debounce.effective
                || startup_grace
                || now.saturating_duration_since(debounce.active_since.unwrap_or(now))
                    >= min_active;
            if debounce.effective {
                AudioTransition::Assert(kind)
            } else {
                AudioTransition::Deassert(kind)
            }
        } else {
            debounce.active_since = None;
            debounce.effective = false;
            AudioTransition::Deassert(kind)
        }
    }

    fn reset_debounce(&mut self) {
        self.playback = KindDebounce::default();
        self.call = KindDebounce::default();
    }

    const fn deassert_all() -> [AudioTransition; 2] {
        [
            AudioTransition::Deassert(InhibitorKind::AudioPlayback),
            AudioTransition::Deassert(InhibitorKind::Call),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioPolicy, AudioTransition, ProbeOutcome};
    use dormant_core::rules::InhibitorKind;
    use std::time::{Duration, Instant};

    const MIN_ACTIVE: Duration = Duration::from_secs(10);
    const BREAKER_COOLDOWN: Duration = Duration::from_secs(60);

    #[derive(Clone, Copy)]
    struct Step<'a> {
        advance: Duration,
        outcome: ProbeOutcome,
        expected: &'a [AudioTransition],
        breaker_open: bool,
        cooldown: Option<Duration>,
        reload: bool,
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the table keeps each timing sequence and its expected transitions adjacent"
    )]
    fn timing_policy_cases_are_deterministic() {
        let deassert_both = [
            AudioTransition::Deassert(InhibitorKind::AudioPlayback),
            AudioTransition::Deassert(InhibitorKind::Call),
        ];
        let assert_playback = [
            AudioTransition::Assert(InhibitorKind::AudioPlayback),
            AudioTransition::Deassert(InhibitorKind::Call),
        ];
        let breaker_open = [
            AudioTransition::Deassert(InhibitorKind::AudioPlayback),
            AudioTransition::Deassert(InhibitorKind::Call),
            AudioTransition::OpenBreaker,
        ];
        let breaker_closed = [
            AudioTransition::CloseBreaker,
            AudioTransition::Deassert(InhibitorKind::AudioPlayback),
            AudioTransition::Deassert(InhibitorKind::Call),
        ];
        let no_change = [AudioTransition::NoChange];

        let cases = [
            (
                "startup grace asserts an existing stream immediately",
                vec![Step {
                    advance: Duration::ZERO,
                    outcome: ProbeOutcome::classified(true, false),
                    expected: &assert_playback,
                    breaker_open: false,
                    cooldown: None,
                    reload: false,
                }],
            ),
            (
                "min active delays a stream that begins after startup",
                vec![
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(false, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: MIN_ACTIVE
                            .checked_sub(Duration::from_millis(1))
                            .expect("test duration is below min_active"),
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_millis(1),
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                ],
            ),
            (
                "deassertion is immediate after an asserted stream stops",
                vec![
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(false, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                ],
            ),
            (
                "two failures deassert without opening the breaker",
                vec![
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::Failure,
                        expected: &no_change,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::Failure,
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: MIN_ACTIVE,
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                ],
            ),
            (
                "reap cap opens a breaker with a cooldown",
                vec![
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::ReapCapReached,
                        expected: &breaker_open,
                        breaker_open: true,
                        cooldown: Some(BREAKER_COOLDOWN),
                        reload: false,
                    },
                    Step {
                        advance: BREAKER_COOLDOWN,
                        outcome: ProbeOutcome::classified(false, false),
                        expected: &breaker_closed,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                ],
            ),
            (
                "playback and call debounce independently",
                vec![
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(false, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(true, true),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(9),
                        outcome: ProbeOutcome::classified(true, true),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(true, true),
                        expected: &[
                            AudioTransition::Assert(InhibitorKind::AudioPlayback),
                            AudioTransition::Assert(InhibitorKind::Call),
                        ],
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                ],
            ),
            (
                "reload starts a fresh policy with fresh startup grace",
                vec![
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(false, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::from_secs(1),
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &deassert_both,
                        breaker_open: false,
                        cooldown: None,
                        reload: false,
                    },
                    Step {
                        advance: Duration::ZERO,
                        outcome: ProbeOutcome::classified(true, false),
                        expected: &assert_playback,
                        breaker_open: false,
                        cooldown: None,
                        reload: true,
                    },
                ],
            ),
        ];

        for (name, steps) in cases {
            let base = Instant::now();
            let mut now = base;
            let mut policy = AudioPolicy::new(MIN_ACTIVE, BREAKER_COOLDOWN);

            for step in steps {
                if step.reload {
                    policy = AudioPolicy::new(MIN_ACTIVE, BREAKER_COOLDOWN);
                }
                now += step.advance;
                assert_eq!(policy.step(now, step.outcome), step.expected, "{name}");
                assert_eq!(policy.breaker_is_open(), step.breaker_open, "{name}");
                assert_eq!(policy.cooldown_remaining(now), step.cooldown, "{name}");
            }
        }
    }
}
