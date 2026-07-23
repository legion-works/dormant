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
}

impl CoordRecord {
    fn seeded() -> Self {
        Self {
            owned: true,
            has_successful_input_read: false,
            input_code: None,
            panel_state: None,
            consecutive_failures: 0,
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
    pub fn record_success(
        &self,
        display: &DisplayId,
        observed: u8,
        expected: u8,
        panel_state: Option<PanelState>,
    ) -> Option<bool> {
        let mut records = self.records.write().unwrap_or_else(PoisonError::into_inner);
        let record = records.get_mut(display)?;
        let prior_owned = record.owned;
        record.owned = observed == expected;
        record.has_successful_input_read = true;
        record.input_code = Some(observed);
        record.panel_state = panel_state;
        record.consecutive_failures = 0;
        (prior_owned != record.owned).then_some(prior_owned)
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
}
