//! Deterministic scheduling policy for systemd watchdog probes.

use std::time::Duration;
use tokio::time::Instant;

/// Deadline policy for the watchdog probe arm.
///
/// The caller owns health checks and the resulting `WATCHDOG=1` decision;
/// this type only spaces probe attempts and deliberately delays after a late
/// tick rather than replaying missed work.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchdogSchedule {
    period: Duration,
    next_deadline: Instant,
}

impl WatchdogSchedule {
    /// Start with a probe due immediately, preserving Tokio interval startup
    /// semantics from the previous run loop.
    #[must_use]
    pub(crate) const fn new(period: Duration, now: Instant) -> Self {
        Self {
            period,
            next_deadline: now,
        }
    }

    /// Return the next time the run loop should probe the active engine.
    #[must_use]
    pub(crate) const fn deadline(&self) -> Instant {
        self.next_deadline
    }

    /// Record one completed probe and delay the next one from this tick.
    pub(crate) fn record_tick(&mut self, now: Instant) {
        self.next_deadline = now + self.period;
    }
}

#[cfg(test)]
mod tests {
    use super::WatchdogSchedule;
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn initial_deadline_is_immediate() {
        let now = Instant::now();
        let schedule = WatchdogSchedule::new(Duration::from_secs(30), now);

        // `tokio::time::interval` fired as soon as `run_loop` first polled it;
        // preserve that startup probe rather than delay systemd readiness by one period.
        assert_eq!(schedule.deadline(), now);
    }

    #[test]
    fn healthy_ticks_keep_a_fixed_cadence() {
        let start = Instant::now();
        let period = Duration::from_secs(30);
        let mut schedule = WatchdogSchedule::new(period, start);

        schedule.record_tick(start);
        assert_eq!(schedule.deadline(), start + period);

        let second_tick = start + period;
        schedule.record_tick(second_tick);
        assert_eq!(schedule.deadline(), second_tick + period);
    }

    #[test]
    fn late_tick_delays_instead_of_bursting_missed_pings() {
        let start = Instant::now();
        let period = Duration::from_secs(30);
        let mut schedule = WatchdogSchedule::new(period, start);
        let late_tick = start + (period * 3);

        schedule.record_tick(late_tick);

        assert_eq!(schedule.deadline(), late_tick + period);
    }
}
