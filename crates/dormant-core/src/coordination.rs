//! Daemon-lifetime ownership verdicts for displays shared across instances.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crate::ownership::OwnershipGate;
use crate::peers::DiscoverAnnounce;
use crate::traits::PanelState;
use crate::types::DisplayId;

/// Interval used to rate-limit logs while shared-display input polling fails.
pub const COORD_POLL_FAILING_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Outcome of one input-source observation fed through the loss-debounce path.
///
/// The poll task logs disagreement / deferred-loss signals from this struct and
/// only sends an `OwnershipPoll` control message when `committed_prior_owned`
/// is `Some(_)`.
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
    /// Instance ID of the peer that last claimed ownership of this display.
    /// `None` when this instance owns the display or the owner is unknown.
    /// Populated by the claim runtime on ownership transitions.
    pub owner_instance_id: Option<String>,
    /// Consecutive agreeing "not mine" readings observed while the cached
    /// verdict was `owned = true`. Reset to 0 on any disagreement or any
    /// subsequent "mine" reading. Triggers a committed ownership loss once
    /// it reaches the configured `loss_confirmations` threshold.
    pub pending_loss_count: u32,
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
            owner_instance_id: None,
            pending_loss_count: 0,
            last_observed_code: None,
        }
    }
}

/// Cloneable, daemon-lifetime cache of shared-display ownership verdicts.
#[derive(Clone, Debug)]
pub struct CoordinationHandle {
    records: Arc<RwLock<HashMap<DisplayId, CoordRecord>>>,
    discovered_peers: Arc<RwLock<HashMap<String, DiscoverAnnounce>>>,
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
            discovered_peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a successful source-input read and return a changed prior verdict.
    ///
    /// Returns `Some(previous_owned)` only when the ownership verdict changed.
    /// Unknown displays are a no-op and return `None`: private displays are not
    /// cached, and a shared display can be removed concurrently with reload.
    ///
    /// This is the single-tick legacy path: it commits a loss on the first
    /// "not mine" reading. The poll task uses [`Self::record_input_observation`]
    /// with the configured `loss_confirmations` to debounce against garbled DDC
    /// reads (issue #134).
    #[allow(clippy::must_use_candidate)] // existing single-tick callers (test setup) fire-and-forget
    pub fn record_success(
        &self,
        display: &DisplayId,
        observed: u8,
        expected: u8,
        panel_state: Option<PanelState>,
    ) -> Option<bool> {
        self.record_input_observation(display, observed, expected, 1, panel_state)
            .committed_prior_owned
    }

    /// Record a successful source-input read through the loss-debounce path
    /// (issue #134).
    ///
    /// Ownership **gain** (`false → true`) commits eagerly: a possibly-wrong
    /// "I own" reading triggers an idempotent wake, which the next poll
    /// re-confirms. This asymmetry with **loss** is intentional — blanking
    /// the panel on a corrupted read strands the operator looking at a dark
    /// screen (live incident journal: 38 ownership flips / 30 min on the
    /// operator's hardware with two daemons polling one panel).
    ///
    /// Ownership **loss** (`true → false`) requires `loss_confirmations`
    /// consecutive agreeing "not mine" readings before the verdict flips.
    /// Any disagreement between consecutive observations resets the pending
    /// counter and is surfaced through `disagreement_with` so the operator
    /// can distinguish "the bus is returning inconsistent values" from
    /// "the input really switched".
    ///
    /// Returns `None` for unknown displays (private displays are never
    /// cached; a shared display can be removed concurrently with reload).
    #[must_use]
    pub fn record_input_observation(
        &self,
        display: &DisplayId,
        observed: u8,
        expected: u8,
        loss_confirmations: u32,
        panel_state: Option<PanelState>,
    ) -> InputObservationOutcome {
        let mut records = self.records.write().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = records.get_mut(display) else {
            return InputObservationOutcome::default();
        };
        let mut outcome = InputObservationOutcome::default();
        let prior_owned = record.owned;
        let new_owned = observed == expected;

        // Disagreement: the freshly observed code differs from the last
        // successful observation for this display. The pending-loss counter
        // resets on every disagreement so two different not-mine codes in
        // a row cannot collude to commit a loss.
        if let Some(previous_code) = record.last_observed_code
            && previous_code != observed
        {
            outcome.disagreement_with = Some(previous_code);
            record.pending_loss_count = 0;
        }

        match (prior_owned, new_owned) {
            (false, false) => {
                // Already not owned; the counter and verdict stay put.
                record.pending_loss_count = 0;
            }
            (true, true) => {
                // Already owned and still reading mine; counter resets.
                record.pending_loss_count = 0;
            }
            (false, true) => {
                // GAIN — eager by design (see doc comment above).
                record.owned = true;
                record.pending_loss_count = 0;
                outcome.committed_prior_owned = Some(prior_owned);
            }
            (true, false) => {
                // Candidate LOSS — debounce.
                record.pending_loss_count = record.pending_loss_count.saturating_add(1);
                if record.pending_loss_count >= loss_confirmations.max(1) {
                    record.owned = false;
                    record.pending_loss_count = 0;
                    outcome.committed_prior_owned = Some(prior_owned);
                } else {
                    outcome.deferred_loss_count = Some(record.pending_loss_count);
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

    /// Record which remote peer currently owns a shared display.
    /// Called by the claim runtime when the local instance loses ownership
    /// (a remote peer claimed it) or when ownership is observed via polling.
    pub fn set_owner(&self, display: &DisplayId, instance_id: Option<String>) {
        let mut records = self.records.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = records.get_mut(display) {
            record.owner_instance_id = instance_id;
        }
    }

    /// Record an mDNS-discovered pairing peer independently of display ownership.
    pub fn upsert_discovered_peer(&self, peer: DiscoverAnnounce) {
        self.discovered_peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer.instance_id.clone(), peer);
    }

    /// Remove an mDNS peer that is no longer advertised without changing ownership.
    pub fn expire_discovered_peer(&self, instance_id: &str) {
        self.discovered_peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(instance_id);
    }

    /// Return the current non-persistent mDNS discovery snapshot.
    #[must_use]
    pub fn discovered_peers(&self) -> HashMap<String, DiscoverAnnounce> {
        self.discovered_peers
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

    use super::{CoordinationGate, CoordinationHandle};
    use crate::ownership::OwnershipGate;
    use crate::traits::{PanelState, PowerState};
    use crate::types::DisplayId;

    fn display(id: &str) -> DisplayId {
        DisplayId(id.into())
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

    // ── issue #134 — ownership-loss debounce against garbled DDC reads ────────

    /// Stray garbled read embedded in an owned sequence does not flip the
    /// verdict — a single misread of `0x60` while the input is still ours
    /// must not blank the panel. Anchored on the live #134 journal evidence
    /// (six direct `ddcutil getvcp 60` samples were stable sl=0x0f while the
    /// daemon was logging garbled values).
    #[test]
    fn garbled_read_in_owned_sequence_does_not_change_verdict() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");

        // owned: 0x11 (mine), 0x11 (mine), 0x99 (garbled stray), 0x11 (mine)
        let outcome = handle.record_input_observation(&aoc, 0x11, 0x11, 3, None);
        assert!(outcome.committed_prior_owned.is_none());
        let outcome = handle.record_input_observation(&aoc, 0x11, 0x11, 3, None);
        assert!(outcome.committed_prior_owned.is_none());
        let outcome = handle.record_input_observation(&aoc, 0x99, 0x11, 3, None);
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
        let outcome = handle.record_input_observation(&aoc, 0x11, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, None);
        assert_eq!(outcome.disagreement_with, Some(0x99));
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Sustained transition (the input really did switch) commits exactly one
    /// loss after `loss_confirmations` consecutive agreeing "not mine" reads.
    /// Subsequent stable "not mine" readings do NOT emit further changes.
    #[test]
    fn sustained_other_input_commits_exactly_one_loss() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");

        // First not-mine reading — counter advances, verdict still owned.
        let outcome = handle.record_input_observation(&aoc, 0x12, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert!(handle.snapshot()[&aoc].owned);

        // Second consecutive agreeing not-mine reading — still pending.
        let outcome = handle.record_input_observation(&aoc, 0x12, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(2));
        assert!(handle.snapshot()[&aoc].owned);

        // Third consecutive agreeing not-mine reading — the verdict flips once.
        let outcome = handle.record_input_observation(&aoc, 0x12, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(true));
        assert_eq!(outcome.deferred_loss_count, None);
        assert!(!handle.snapshot()[&aoc].owned);

        // A fourth consecutive agreeing not-mine reading — already not owned,
        // no further transition. The pending counter must reset.
        let outcome = handle.record_input_observation(&aoc, 0x12, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, None);
        assert!(!handle.snapshot()[&aoc].owned);
    }

    /// Disagreement between consecutive not-mine codes (e.g., the bus returned
    /// two different non-matching codes in a row) must hold the prior verdict —
    /// neither commit a loss nor advance the pending-loss counter past the
    /// freshly-observed disagreement. Anchored on the #134 cross-machine DDC
    /// traffic pattern where successful-but-wrong reads vary.
    #[test]
    fn disagreeing_not_mine_reads_hold_prior_verdict() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");

        // Garbled reading #1: pending=1, no commitment.
        let outcome = handle.record_input_observation(&aoc, 0x12, 0x11, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert!(handle.snapshot()[&aoc].owned);

        // Garbled reading #2 with a DIFFERENT not-mine code: counter resets to
        // 1 (fresh disagreement), verdict stays owned.
        let outcome = handle.record_input_observation(&aoc, 0x13, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, Some(1));
        assert_eq!(outcome.disagreement_with, Some(0x12));
        assert!(handle.snapshot()[&aoc].owned);

        // A third agreeing garbled reading of 0x13 advances the counter to 2,
        // still below the 3-confirmation threshold — verdict still owned.
        let outcome = handle.record_input_observation(&aoc, 0x13, 0x11, 3, None);
        assert_eq!(outcome.deferred_loss_count, Some(2));
        assert_eq!(outcome.disagreement_with, None);
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Ownership *gain* stays eager — waking on a possibly-wrong "I own" read
    /// is harmless (the wake path is idempotent and re-confirmed by the next
    /// poll). The asymmetry with loss (which blanks) is intentional and
    /// documented on `record_input_observation`.
    #[test]
    fn ownership_gain_is_eager_with_no_confirmation() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");

        // Drive to not-owned via the confirmed-loss path (3 agreeing readings).
        for _ in 0..3 {
            let _ = handle.record_input_observation(&aoc, 0x12, 0x11, 3, None);
        }
        assert!(!handle.snapshot()[&aoc].owned);

        // A single "mine" reading immediately flips the verdict back to owned.
        let outcome = handle.record_input_observation(&aoc, 0x11, 0x11, 3, None);
        assert_eq!(outcome.committed_prior_owned, Some(false));
        assert_eq!(outcome.deferred_loss_count, None);
        assert!(handle.snapshot()[&aoc].owned);
    }

    /// Single-not-mine commits a loss when `loss_confirmations == 1` —
    /// preserves the legacy one-tick semantics for setups that explicitly
    /// opt out of debouncing.
    #[test]
    fn loss_confirmations_one_preserves_legacy_single_tick_semantics() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");

        let outcome = handle.record_input_observation(&aoc, 0x12, 0x11, 1, None);
        assert_eq!(outcome.committed_prior_owned, Some(true));
        assert!(!handle.snapshot()[&aoc].owned);
    }

    /// Disagreement detected on a transition from owned to mine after a garbled
    /// window — the fresh "mine" reading disagrees with the last observed code
    /// (the garbled one) and must reset the pending counter while leaving the
    /// verdict untouched (it was never flipped during the garble).
    #[test]
    fn return_to_mine_after_garbled_window_holds_verdict_and_resets_counter() {
        let handle = CoordinationHandle::new([display("aoc")]);
        let aoc = display("aoc");

        let _ = handle.record_input_observation(&aoc, 0x99, 0x11, 3, None); // garbled #1
        let _ = handle.record_input_observation(&aoc, 0x9a, 0x11, 3, None); // garbled #2 (differs)
        let outcome = handle.record_input_observation(&aoc, 0x11, 0x11, 3, None);
        // 0x11 disagrees with the last observed 0x9a, the verdict was never
        // flipped, and the counter must reset to 0.
        assert_eq!(outcome.committed_prior_owned, None);
        assert_eq!(outcome.deferred_loss_count, None);
        assert_eq!(outcome.disagreement_with, Some(0x9a));
        assert!(handle.snapshot()[&aoc].owned);
    }
}
