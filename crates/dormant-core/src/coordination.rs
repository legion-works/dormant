//! Daemon-lifetime ownership verdicts for displays shared across instances.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crate::ownership::OwnershipGate;
use crate::traits::PanelState;
use crate::types::DisplayId;

/// Interval used to rate-limit logs while shared-display input polling fails.
pub const COORD_POLL_FAILING_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Mapping of local and peer input-source codes used to classify a raw VCP 0x60
/// reading. The local write code can alias a different read-back value (e.g. the
/// operator's AOC panel is written with `0x15` but reads back `0x10`); both
/// classify as [`InputSourceObservation::Local`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputCodeAliases {
    /// The VCP 0x60 code that the panel reports when this instance's input is
    /// selected.
    pub local_read: u8,
    /// The VCP 0x60 write code used to select this instance's input (may differ
    /// from `local_read` on panels that alias the write register).
    pub local_write: u8,
    /// The VCP 0x60 code reported when the configured peer's input is selected.
    pub peer_read: Option<u8>,
    /// The VCP 0x60 write code for the peer input. `None` when unknown.
    pub peer_write: Option<u8>,
}

/// Classification of a raw VCP 0x60 input-source value against the configured
/// local and peer aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSourceObservation {
    /// The input is currently the local machine's source.
    Local,
    /// The input is currently a configured peer's source.
    Peer,
    /// An unrecognised code — repeatedly identical unknowns may confirm a loss;
    /// differing unknowns reset.
    Unknown(u8),
}

impl InputCodeAliases {
    /// Classify a raw `0x60` value against the configured alias maps.
    #[must_use]
    pub fn classify(&self, code: u8) -> InputSourceObservation {
        if code == self.local_read || code == self.local_write {
            return InputSourceObservation::Local;
        }
        if self.peer_read == Some(code) || self.peer_write == Some(code) {
            return InputSourceObservation::Peer;
        }
        InputSourceObservation::Unknown(code)
    }
}

/// Outcome of one input-source observation fed through the symmetric debounce
/// path (gain and loss both deferred).
///
/// The poll task logs disagreement / deferred signals from this struct and only
/// sends an `OwnershipPoll` control message when `committed_prior_owned` is
/// `Some(_)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputObservationOutcome {
    /// The verdict that was committed on this observation, plus the prior
    /// verdict. `Some(prior)` means a real ownership transition was committed
    /// and the rules engine should be re-consulted; `None` means the cached
    /// verdict was held (gain or loss was deferred or reads disagreed).
    pub committed_prior_owned: Option<bool>,
    /// `Some(pending_count)` when a potential loss was observed but is still
    /// pending further confirming reads (the cached verdict is still owned).
    pub deferred_loss_count: Option<u32>,
    /// `Some(pending_count)` when a potential gain was observed but is still
    /// pending further confirming reads (the cached verdict is still not-owned).
    pub deferred_gain_count: Option<u32>,
    /// `Some(previous_code)` when the freshly observed code disagreed with the
    /// last successful observation for this display. The disagreement signal
    /// lets the operator distinguish "poll is healthy" from "the bus is
    /// returning inconsistent values" without parsing the cache.
    pub disagreement_with: Option<u8>,
}

/// Last known ownership and readback state for one shared display.
#[derive(Debug, Clone, PartialEq)]
pub struct CoordRecord {
    /// Whether this instance may currently control the display.
    pub owned: bool,
    /// Whether any input-source read has succeeded since this record was seeded.
    pub has_successful_input_read: bool,
    /// Last successfully observed input-source code.
    pub input_code: Option<u8>,
    /// Panel state observed alongside the last successful input-source read.
    pub panel_state: Option<PanelState>,
    /// Number of consecutive failed input-source reads since the last success.
    pub consecutive_failures: u32,
    /// Consecutive agreeing observations toward a pending verdict change.
    /// Tracks both gain and loss. Reset to 0 on any raw-code disagreement
    /// or on return-to-current-verdict readings.
    pub pending_transition_count: u32,
    /// The raw code being tracked for the pending transition.
    /// `None` when no transition is pending. Repeated identical codes confirm
    /// the transition; differing codes reset the count to 1.
    pub pending_transition_code: Option<u8>,
    /// Last successful observation's raw input code; used to detect
    /// disagreements between consecutive successful reads (issue #134).
    pub last_observed_code: Option<u8>,
}

impl CoordRecord {
    fn seeded() -> Self {
        Self {
            owned: true,
            has_successful_input_read: false,
            input_code: None,
            panel_state: None,
            consecutive_failures: 0,
            pending_transition_count: 0,
            pending_transition_code: None,
            last_observed_code: None,
        }
    }
}

/// Cloneable, daemon-lifetime cache of shared-display ownership verdicts.
#[derive(Clone, Debug)]
pub struct CoordinationHandle {
    #[doc(hidden)]
    pub records: Arc<RwLock<HashMap<DisplayId, CoordRecord>>>,
}

impl CoordinationHandle {
    /// Create a cache with an owned material record for every shared display.
    #[must_use]
    pub fn new(shared: impl IntoIterator<Item = DisplayId>) -> Self {
        let records = shared
            .into_iter()
            .map(|display| (display, CoordRecord::seeded()))
            .collect();
        Self {
            records: Arc::new(RwLock::new(records)),
        }
    }

    /// Record a successful source-input read and return a changed prior verdict.
    ///
    /// Returns `Some(previous_owned)` only when the ownership verdict changed.
    /// Unknown displays are a no-op and return `None`: private displays are not
    /// cached, and a shared display can be removed concurrently with reload.
    ///
    /// This is the single-tick legacy path: `confirmations = 1` commits both
    /// gain and loss on the first reading. Callers exercising the debounce path
    /// should use [`Self::record_input_observation`] with their configured
    /// threshold.
    #[allow(clippy::must_use_candidate)] // existing single-tick callers (test setup) fire-and-forget
    pub fn record_success(
        &self,
        display: &DisplayId,
        observed: u8,
        expected: u8,
        panel_state: Option<PanelState>,
    ) -> Option<bool> {
        let aliases = InputCodeAliases {
            local_read: expected,
            local_write: expected,
            peer_read: None,
            peer_write: None,
        };
        self.record_input_observation(display, observed, &aliases, 1, panel_state)
            .committed_prior_owned
    }

    /// Record a successful source-input read through the symmetric debounce
    /// path (issue #134 extended to cover both gain and loss).
    ///
    /// Both ownership **gain** and **loss** are deferred — `confirmations`
    /// consecutive agreeing "mine" or "not-mine" readings are required before
    /// the verdict commits. Any raw-code disagreement between consecutive
    /// observations resets the pending counter and is surfaced through
    /// `disagreement_with`. This symmetric design prevents the panel from
    /// blanking on a corrupted read (loss is debounced) and prevents spurious
    /// wake cycles on transient "mine" readings from a garbled bus (gain is
    /// debounced).
    ///
    /// Differing unknown codes reset the pending transition; repeated identical
    /// unknowns may confirm a loss. A read failure (handled separately via
    /// [`Self::record_failure`]) never changes the verdict.
    ///
    /// Returns `InputObservationOutcome::default()` for unknown displays
    /// (private displays are never cached; a shared display can be removed
    /// concurrently with reload).
    #[must_use]
    pub fn record_input_observation(
        &self,
        display: &DisplayId,
        observed: u8,
        aliases: &InputCodeAliases,
        confirmations: u32,
        panel_state: Option<PanelState>,
    ) -> InputObservationOutcome {
        let mut records = self.records.write().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = records.get_mut(display) else {
            return InputObservationOutcome::default();
        };
        let mut outcome = InputObservationOutcome::default();
        let prior_owned = record.owned;
        let classification = aliases.classify(observed);
        let is_local = matches!(classification, InputSourceObservation::Local);

        // Disagreement: freshly observed raw code differs from the last
        // successful observation. This resets any pending transition — two
        // different codes in a row cannot collude to commit a transition.
        // The pending-transition code and count are cleared so the next
        // observation (if it initiates a new transition) starts from 1.
        if let Some(previous_code) = record.last_observed_code
            && previous_code != observed
        {
            outcome.disagreement_with = Some(previous_code);
            record.pending_transition_count = 0;
            record.pending_transition_code = None;
        }

        match (prior_owned, is_local) {
            (false, false) | (true, true) => {
                // Verdict already matches the observation — no transition
                // candidate. Reset any pending state.
                record.pending_transition_count = 0;
                record.pending_transition_code = None;
            }
            (false, true) => {
                // Candidate GAIN — heading toward owned.
                if record.pending_transition_code == Some(observed) {
                    record.pending_transition_count =
                        record.pending_transition_count.saturating_add(1);
                } else {
                    record.pending_transition_code = Some(observed);
                    record.pending_transition_count = 1;
                }
                let threshold = confirmations.max(1);
                if record.pending_transition_count >= threshold {
                    record.owned = true;
                    record.pending_transition_count = 0;
                    record.pending_transition_code = None;
                    outcome.committed_prior_owned = Some(prior_owned);
                } else {
                    outcome.deferred_gain_count = Some(record.pending_transition_count);
                }
            }
            (true, false) => {
                // Candidate LOSS — heading toward not-owned.
                if record.pending_transition_code == Some(observed) {
                    record.pending_transition_count =
                        record.pending_transition_count.saturating_add(1);
                } else {
                    record.pending_transition_code = Some(observed);
                    record.pending_transition_count = 1;
                }
                let threshold = confirmations.max(1);
                if record.pending_transition_count >= threshold {
                    record.owned = false;
                    record.pending_transition_count = 0;
                    record.pending_transition_code = None;
                    outcome.committed_prior_owned = Some(prior_owned);
                } else {
                    outcome.deferred_loss_count = Some(record.pending_transition_count);
                }
            }
        }

        record.has_successful_input_read = true;
        record.input_code = Some(observed);
        record.panel_state = panel_state;
        record.consecutive_failures = 0;
        record.last_observed_code = Some(observed);
        outcome
    }

    /// Record an input-source read failure without changing the ownership verdict.
    ///
    /// Unknown displays are a no-op: private displays are not cached, and a
    /// shared display can be removed concurrently with reload.
    pub fn record_failure(&self, display: &DisplayId) {
        if let Some(record) = self
            .records
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(display)
        {
            record.consecutive_failures = record.consecutive_failures.saturating_add(1);
        }
    }

    /// Immediately mark a display as owned — called after a verified-successful
    /// local write (issue #139).  The machine has first-hand proof that it owns
    /// the panel; waiting for the debounced poll to independently rediscover
    /// this fact adds ~`loss_confirmations × poll_interval` of visible lag.
    ///
    /// The write-and-verify path is observation, not authority — the poll must
    /// still never cause a write.  This method feeds the *result* of a write the
    /// machine performed, which is a different category.
    ///
    /// Idempotent: if the display is already marked owned, this is a no-op.
    /// Unknown displays (private, or concurrently removed) are silently ignored.
    pub fn mark_owned_immediate(&self, display: &DisplayId) {
        let mut records = self.records.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = records.get_mut(display)
            && !record.owned
        {
            record.owned = true;
            // Clear any pending transition so the poller's subsequent
            // agreeing reads don't double-fire or register a disagreement.
            record.pending_transition_count = 0;
            record.pending_transition_code = None;
            record.consecutive_failures = 0;
        }
    }

    /// Reconcile shared displays after reload without resetting surviving records.
    pub fn reconcile_shared(&self, shared: impl IntoIterator<Item = DisplayId>) {
        let shared: HashSet<_> = shared.into_iter().collect();
        let mut records = self.records.write().unwrap_or_else(PoisonError::into_inner);
        records.retain(|display, _| shared.contains(display));
        for display in shared {
            records.entry(display).or_insert_with(CoordRecord::seeded);
        }
    }

    /// Return a point-in-time copy of every shared-display record.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<DisplayId, CoordRecord> {
        self.records
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn seeded_ids(&self) -> Vec<DisplayId> {
        let mut displays: Vec<_> = self
            .records
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        displays.sort();
        displays
    }

    /// Whether any input-source read has succeeded since this display was seeded.
    /// Test-only observability — production ownership decisions must use
    /// [`CoordinationGate::owns`].
    #[doc(hidden)]
    #[must_use]
    pub fn has_successful_read(&self, display: &DisplayId) -> bool {
        self.records
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(display)
            .is_some_and(|record| record.has_successful_input_read)
    }
}

/// Ownership gate backed by the daemon-lifetime shared-display cache.
#[derive(Clone, Debug)]
pub struct CoordinationGate {
    handle: CoordinationHandle,
}

impl CoordinationGate {
    /// Create a gate that reads ownership verdicts from `handle`.
    #[must_use]
    pub fn new(handle: CoordinationHandle) -> Self {
        Self { handle }
    }
}

impl OwnershipGate for CoordinationGate {
    fn owns(&self, display: &DisplayId) -> bool {
        self.handle
            .records
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(display)
            .is_none_or(|record| record.owned)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{CoordinationGate, CoordinationHandle, InputCodeAliases, InputSourceObservation};
    use crate::ownership::OwnershipGate;
    use crate::traits::{PanelState, PowerState};
    use crate::types::DisplayId;

    fn display(id: &str) -> DisplayId {
        DisplayId(id.into())
    }

    /// Build aliases where only a single local-read code determines ownership.
    fn aliases(local: u8) -> InputCodeAliases {
        InputCodeAliases {
            local_read: local,
            local_write: local,
            peer_read: None,
            peer_write: None,
        }
    }

    #[test]
    fn coordination_gate_seeds_every_shared_display_owned() {
        let gate = CoordinationGate::new(CoordinationHandle::new([display("aoc"), display("tv")]));

        assert!(gate.owns(&display("aoc")));
        assert!(gate.owns(&display("tv")));
    }

    #[test]
    fn seeded_ids_proves_shared_entry_exists_before_first_poll() {
        let handle = CoordinationHandle::new([display("aoc")]);

        assert_eq!(handle.seeded_ids(), vec![display("aoc")]);
    }

    #[test]
    fn private_missing_entry_is_always_owned() {
        let gate = CoordinationGate::new(CoordinationHandle::new([display("aoc")]));

        assert!(gate.owns(&display("private")));
    }

    #[test]
    fn successful_other_input_changes_false_and_returns_previous_true() {
        let handle = CoordinationHandle::new([display("aoc")]);

        assert_eq!(
            handle.record_success(&display("aoc"), 2, 1, None),
            Some(true)
        );
        assert!(!CoordinationGate::new(handle).owns(&display("aoc")));
    }

    #[test]
    fn transient_failure_holds_false_verdict() {
        let handle = CoordinationHandle::new([display("aoc")]);
        handle.record_success(&display("aoc"), 2, 1, None);

        handle.record_failure(&display("aoc"));
        handle.record_failure(&display("aoc"));

        assert!(!CoordinationGate::new(handle).owns(&display("aoc")));
    }

    #[test]
    fn cold_start_failure_retains_no_successful_read_marker() {
        let handle = CoordinationHandle::new([display("aoc")]);

        handle.record_failure(&display("aoc"));

        assert!(CoordinationGate::new(handle.clone()).owns(&display("aoc")));
        assert!(!handle.has_successful_read(&display("aoc")));
    }

    #[test]
    fn reload_reconcile_retains_survivors_seeds_additions_and_drops_removed() {
        let aoc = display("aoc");
        let tv = display("tv");
        let projector = display("projector");
        let handle = CoordinationHandle::new([aoc.clone(), tv]);
        handle.record_success(
            &aoc,
            2,
            1,
            Some(PanelState {
                power: Some(PowerState::On),
                brightness: Some(42),
            }),
        );
        handle.record_failure(&aoc);
        handle.record_failure(&aoc);
        let survivor = handle.snapshot()[&aoc].clone();

        handle.reconcile_shared([aoc.clone(), projector.clone()]);

        assert_eq!(handle.seeded_ids(), vec![aoc.clone(), projector.clone()]);
        assert_eq!(handle.snapshot()[&aoc], survivor);
        assert!(!CoordinationGate::new(handle.clone()).owns(&aoc));
        assert!(CoordinationGate::new(handle.clone()).owns(&projector));
        assert!(!handle.has_successful_read(&projector));
        assert!(CoordinationGate::new(handle).owns(&display("tv")));
    }

    #[test]
    fn poisoned_cache_recovers_for_read_and_write() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let poisoned = Arc::clone(&handle.records);
        let thread = std::thread::spawn(move || {
            let _guard = poisoned.write().expect("lock is initially healthy");
            panic!("poison the cache");
        });
        assert!(thread.join().is_err());

        assert!(CoordinationGate::new(handle.clone()).owns(&display("aoc")));
        assert_eq!(
            handle.record_success(&display("aoc"), 2, 1, None),
            Some(true)
        );
        assert!(!CoordinationGate::new(handle).owns(&display("aoc")));
    }

    // ── input-code aliases ────────────────────────────────────────────

    #[test]
    fn local_read_and_write_codes_both_classify_as_local() {
        let a = InputCodeAliases {
            local_read: 0x10,
            local_write: 0x15,
            peer_read: None,
            peer_write: None,
        };
        assert_eq!(a.classify(0x10), InputSourceObservation::Local);
        assert_eq!(a.classify(0x15), InputSourceObservation::Local);
    }

    #[test]
    fn peer_read_and_write_codes_classify_as_peer() {
        let a = InputCodeAliases {
            local_read: 0x10,
            local_write: 0x10,
            peer_read: Some(0x20),
            peer_write: Some(0x25),
        };
        assert_eq!(a.classify(0x20), InputSourceObservation::Peer);
        assert_eq!(a.classify(0x25), InputSourceObservation::Peer);
    }

    #[test]
    fn unrecognised_code_is_unknown_with_raw_value_preserved() {
        let a = aliases(0x10);
        assert_eq!(a.classify(0xFF), InputSourceObservation::Unknown(0xFF));
        // On the AOC panel, a garbled read returns 0x00 — we must preserve it.
        assert_eq!(a.classify(0x00), InputSourceObservation::Unknown(0x00));
    }

    #[test]
    fn local_takes_precedence_over_peer_when_codes_overlap() {
        // Degenerate but possible: both local and peer claim the same raw code.
        // Local wins so a machine always recognises its own input.
        let a = InputCodeAliases {
            local_read: 0x10,
            local_write: 0x10,
            peer_read: Some(0x10),
            peer_write: None,
        };
        assert_eq!(a.classify(0x10), InputSourceObservation::Local);
    }

    // ── symmetric debounce ────────────────────────────────────────────

    /// Ownership **gain** is now symmetrically debounced with loss — a single
    /// "mine" reading no longer commits a gain. This test FAILS on the pre-Task7
    /// eager-gain code and proves the behavioral change.
    #[test]
    fn observation_gain_requires_confirmations() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let aliases = aliases(0x11);

        // Drive to not-owned via the confirmed-loss path (3 agreeing readings).
        for _ in 0..3 {
            let _ = handle.record_input_observation(&aoc, 0x12, &aliases, 3, None);
        }
        assert!(!handle.snapshot()[&aoc].owned);

        // A single "mine" reading must NOT immediately flip the verdict — gain
        // is now debounced symmetrically with loss (confirmations = 3).
        let outcome = handle.record_input_observation(&aoc, 0x11, &aliases, 3, None);
        assert_eq!(
            outcome.committed_prior_owned, None,
            "gain must be deferred, not eager"
        );
        assert_eq!(outcome.deferred_gain_count, Some(1));
        assert!(
            !handle.snapshot()[&aoc].owned,
            "verdict must stay not-owned until gain confirmations reached"
        );

        // Second confirming "mine" — still deferred.
        let outcome = handle.record_input_observation(&aoc, 0x11, &aliases, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_gain_count, Some(2));
        assert!(!handle.snapshot()[&aoc].owned);

        // Third confirming "mine" — gain committed.
        let outcome = handle.record_input_observation(&aoc, 0x11, &aliases, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(false));
        assert_eq!(outcome.deferred_gain_count, None);
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// A Local reading with a different raw code (e.g. 0x15 write alias vs
    /// 0x10 read alias) still classifies as Local and contributes to gain.
    #[test]
    fn observation_gain_across_local_alias_codes() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let aliases = InputCodeAliases {
            local_read: 0x10,
            local_write: 0x15,
            peer_read: None,
            peer_write: None,
        };

        // Drive to not-owned.
        for _ in 0..3 {
            let _ = handle.record_input_observation(&aoc, 0x12, &aliases, 3, None);
        }
        assert!(!handle.snapshot()[&aoc].owned);

        // 0x10 (local read alias) — gain pending 1/3.
        let outcome = handle.record_input_observation(&aoc, 0x10, &aliases, 3, None);
        assert_eq!(outcome.deferred_gain_count, Some(1));

        // 0x15 (local write alias) — raw code differs → disagreement resets
        // the pending transition, but classification is still Local → a fresh
        // pending-gain counter starts at 1.
        let outcome = handle.record_input_observation(&aoc, 0x15, &aliases, 3, None);
        assert_eq!(outcome.deferred_gain_count, Some(1));
        assert_eq!(outcome.disagreement_with, Some(0x10));
        assert!(!handle.snapshot()[&aoc].owned);
    }

    // ── loss debounce preserved — issue #134 anchor tests ─────────────

    /// Stray garbled read embedded in an owned sequence does not flip the
    /// verdict — a single misread of `0x60` while the input is still ours
    /// must not blank the panel. Anchored on the live #134 journal evidence
    /// (six direct `ddcutil getvcp 60` samples were stable sl=0x0f while the
    /// daemon was logging garbled values).
    #[test]
    fn garbled_read_in_owned_sequence_does_not_change_verdict() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // owned: 0x11 (mine), 0x11 (mine), 0x99 (garbled stray), 0x11 (mine)
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        assert!(outcome.committed_prior_owned.is_none());
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        assert!(outcome.committed_prior_owned.is_none());
        let outcome = handle.record_input_observation(&aoc, 0x99, &al, 3, None);
        // A single stray "not mine" must NOT commit a loss — only the pending
        // counter advances. The verdict stays owned.
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        // Garbled code disagrees with the last observation → emit a disagreement
        // signal so the operator learns something was off.
        assert_eq!(outcome.disagreement_with, Some(0x11));
        assert!(handle.snapshot()[&aoc].owned);

        // A subsequent "mine" reading resets the pending counter and holds the
        // verdict — never blanks. The return-to-mine also disagrees with the
        // garbled observation, so the disagreement signal fires here too.
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, None);
        assert_eq!(outcome.disagreement_with, Some(0x99));
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Sustained transition (the input really did switch) commits exactly one
    /// loss after `confirmations` consecutive agreeing "not mine" readings.
    /// Subsequent stable "not mine" readings do NOT emit further changes.
    #[test]
    fn sustained_other_input_commits_exactly_one_loss() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // First not-mine reading — counter advances, verdict still owned.
        let outcome = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert!(handle.snapshot()[&aoc].owned);

        // Second consecutive agreeing not-mine reading — still pending.
        let outcome = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(2));
        assert!(handle.snapshot()[&aoc].owned);

        // Third consecutive agreeing not-mine reading — the verdict flips once.
        let outcome = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(true));
        assert_eq!(outcome.deferred_loss_count, None);
        assert!(!handle.snapshot()[&aoc].owned);

        // A fourth consecutive agreeing not-mine reading — already not owned,
        // no further transition. The pending counter must reset.
        let outcome = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, None);
        assert!(!handle.snapshot()[&aoc].owned);
    }

    /// Disagreement between consecutive not-mine codes (e.g., the bus returned
    /// two different non-matching codes in a row) must hold the prior verdict —
    /// neither commit a loss nor advance the pending counter past the
    /// freshly-observed disagreement. Anchored on the #134 cross-machine DDC
    /// traffic pattern where successful-but-wrong reads vary.
    #[test]
    fn disagreeing_not_mine_reads_hold_prior_verdict() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // Garbled reading #1: pending=1, no commitment.
        let outcome = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert!(handle.snapshot()[&aoc].owned);

        // Garbled reading #2 with a DIFFERENT not-mine code: counter resets to
        // 1 (fresh disagreement), verdict stays owned.
        let outcome = handle.record_input_observation(&aoc, 0x13, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert_eq!(outcome.disagreement_with, Some(0x12));
        assert!(handle.snapshot()[&aoc].owned);

        // A third agreeing garbled reading of 0x13 advances the counter to 2,
        // still below the 3-confirmation threshold — verdict still owned.
        let outcome = handle.record_input_observation(&aoc, 0x13, &al, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(2));
        assert_eq!(outcome.disagreement_with, None);
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Differing unknown codes (classify as `Unknown(0x99)` then
    /// `Unknown(0xAA)`) must RESET the pending transition — repeated identical
    /// unknowns may confirm a loss, but differing garbled values must not.
    #[test]
    fn observation_differing_unknown_codes_reset_pending_transition() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // Unknown(0x99) — candidate loss, pending=1.
        let outcome = handle.record_input_observation(&aoc, 0x99, &al, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert!(handle.snapshot()[&aoc].owned);

        // Unknown(0xAA) — differs from pending 0x99 → resets, pending=1 fresh.
        let outcome = handle.record_input_observation(&aoc, 0xAA, &al, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert_eq!(outcome.disagreement_with, Some(0x99));
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Repeated identical unknown codes confirm a loss at the threshold.
    #[test]
    fn observation_repeated_identical_unknown_codes_confirm_loss() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // Unknown(0xFE) × 3
        let outcome = handle.record_input_observation(&aoc, 0xFE, &al, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        let outcome = handle.record_input_observation(&aoc, 0xFE, &al, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(2));
        let outcome = handle.record_input_observation(&aoc, 0xFE, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(true));
        assert!(!handle.snapshot()[&aoc].owned);
    }

    /// A read failure does not change the verdict — hold-last invariant.
    #[test]
    fn observation_read_failure_holds_verdict() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // Drive to not-owned.
        for _ in 0..3 {
            let _ = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        }
        assert!(!handle.snapshot()[&aoc].owned);

        // Read failures do not change the verdict.
        handle.record_failure(&aoc);
        handle.record_failure(&aoc);
        handle.record_failure(&aoc);
        assert!(!handle.snapshot()[&aoc].owned);
    }

    /// Single-not-mine commits a loss when `confirmations == 1` —
    /// preserves the legacy one-tick semantics for setups that explicitly
    /// opt out of debouncing.
    #[test]
    fn loss_confirmations_one_preserves_legacy_single_tick_semantics() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        let outcome = handle.record_input_observation(&aoc, 0x12, &al, 1, None);
        assert_eq!(outcome.committed_prior_owned, Some(true));
        assert!(!handle.snapshot()[&aoc].owned);
    }

    /// Single-mine commits a gain when `confirmations == 1` —
    /// preserves the legacy one-tick semantics.
    #[test]
    fn gain_confirmations_one_preserves_legacy_single_tick_semantics() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // Drive to not-owned with confirmations=1.
        let _ = handle.record_input_observation(&aoc, 0x12, &al, 1, None);
        assert!(!handle.snapshot()[&aoc].owned);

        // Single mine reading with confirmations=1 commits gain.
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 1, None);
        assert_eq!(outcome.committed_prior_owned, Some(false));
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Disagreement detected on a transition from owned to mine after a garbled
    /// window — the fresh "mine" reading disagrees with the last observed code
    /// (the garbled one) and must reset the pending counter while leaving the
    /// verdict untouched (it was never flipped during the garble).
    #[test]
    fn return_to_mine_after_garbled_window_holds_verdict_and_resets_counter() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        let _ = handle.record_input_observation(&aoc, 0x99, &al, 3, None); // garbled #1
        let _ = handle.record_input_observation(&aoc, 0x9a, &al, 3, None); // garbled #2 (differs)
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        // 0x11 disagrees with the last observed 0x9a, the verdict was never
        // flipped, and the counter must reset to 0.
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, None);
        assert_eq!(outcome.deferred_gain_count, None);
        assert_eq!(outcome.disagreement_with, Some(0x9a));
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Ownership gain is debounced with the same confirmations threshold as loss.
    /// Three confirming "mine" readings commit the gain; fewer do not.
    #[test]
    fn observation_gain_is_debounced_with_confirmations() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");
        let al = aliases(0x11);

        // Drive to not-owned.
        for _ in 0..3 {
            let _ = handle.record_input_observation(&aoc, 0x12, &al, 3, None);
        }
        assert!(!handle.snapshot()[&aoc].owned);

        // First "mine" — pending gain, no commit.
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_gain_count, Some(1));

        // Second "mine" — still pending.
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_gain_count, Some(2));
        assert!(!handle.snapshot()[&aoc].owned);

        // Third "mine" — gain committed.
        let outcome = handle.record_input_observation(&aoc, 0x11, &al, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(false));
        assert_eq!(outcome.deferred_gain_count, None);
        assert!(handle.snapshot()[&aoc].owned);
    }
}
