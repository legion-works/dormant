//! Causal reload identities and daemon-local diagnostic observations.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::{
    reload::ReloadOutcome,
    state_machine::Phase,
    types::{DisplayId, RuleId},
};

/// SHA-256 fingerprint of exactly the configuration or credentials bytes parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentRevision(String);

impl ContentRevision {
    /// Fingerprint loaded document bytes with SHA-256.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }

    /// Stable sentinel for an optional credentials document that was absent.
    #[must_use]
    pub fn missing() -> Self {
        Self("missing".to_owned())
    }
}

/// Pair of content revisions that define one daemon runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRevision {
    /// Revision of the configuration document.
    pub config: ContentRevision,
    /// Revision of the optional credentials document.
    pub credentials: ContentRevision,
}

/// Monotonic identity for one installed daemon generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GenerationId(pub u64);

/// Origin of a reload request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReloadSource {
    /// Filesystem configuration watcher.
    Watcher,
    /// Local control-plane request.
    Control,
    /// Web configuration apply request.
    WebApply,
    /// IPC request from an external client.
    Ipc,
    /// Operating-system signal.
    Signal,
}

/// Causal record of one reload request or a coalesced batch of requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReceipt {
    /// Request identities folded into this reload attempt.
    pub request_ids: Vec<u64>,
    /// Sources that contributed requests to this reload attempt.
    pub sources: Vec<ReloadSource>,
    /// Revision requested when the batch began.
    pub requested_revision: RuntimeRevision,
    /// Revision actually applied after parsing and validation.
    pub applied_revision: RuntimeRevision,
    /// Generation current after this reload attempt.
    pub generation: GenerationId,
    /// Existing outcome surface for the reload attempt.
    pub outcome: ReloadOutcome,
    /// Whether this receipt combines multiple requests.
    pub coalesced: bool,
}

/// Daemon-local observations for diagnostics and deterministic tests.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonObservation {
    /// A correlated reload batch began processing.
    ReloadStarted {
        /// Request identities in the batch.
        request_ids: Vec<u64>,
        /// Revision requested by the batch.
        requested_revision: RuntimeRevision,
    },
    /// A correlated reload batch completed.
    ReloadCompleted(ReloadReceipt),
    /// A generation finished draining its queued inputs before teardown.
    GenerationDrained {
        /// Generation that reached its drain barrier.
        generation: GenerationId,
    },
    /// A generation was installed and can receive routed inputs.
    GenerationStarted {
        /// Generation that was installed.
        generation: GenerationId,
    },
    /// A display changed lifecycle phase.
    DisplayPhaseChanged {
        /// Generation that owns the rules engine.
        generation: GenerationId,
        /// Rule whose display transitioned; `None` for a manual/rule-less command path.
        rule_id: Option<RuleId>,
        /// Display that transitioned.
        display_id: DisplayId,
        /// Phase before the transition.
        old_phase: Phase,
        /// Phase after the transition.
        new_phase: Phase,
    },
    /// A generation's watchdog emitted a heartbeat.
    WatchdogPing {
        /// Generation that emitted the heartbeat.
        generation: GenerationId,
    },
    /// A corrupt wear ledger could not be safely replaced.
    WearLedgerCorrupt {
        /// Corrupt ledger path.
        path: PathBuf,
        /// Error returned while renaming the corrupt ledger.
        rename_error: String,
    },
    /// Boot fell back from an invalid candidate to a last-known-good configuration.
    BootRollback {
        /// Fingerprint of the failed candidate.
        failed_fingerprint: String,
        /// Fingerprint of the last-known-good configuration.
        lkg_fingerprint: String,
        /// Failure detail from the rejected candidate.
        detail: String,
    },
}

/// Bounded, daemon-owned broadcast hub for non-blocking observations.
#[derive(Debug, Clone)]
pub struct ObservationHub {
    sender: broadcast::Sender<DaemonObservation>,
}

impl ObservationHub {
    /// Create a hub retaining at most `capacity` observations per subscriber.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Subscribe to observations emitted by this hub.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonObservation> {
        self.sender.subscribe()
    }

    /// Emit an observation without waiting for any subscriber.
    pub fn emit(&self, observation: DaemonObservation) {
        let _ = self.sender.send(observation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_revisions_distinguish_loaded_bytes_and_missing_credentials() {
        assert_eq!(
            ContentRevision::from_bytes(b"abc"),
            ContentRevision(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned()
            ),
        );
        assert_ne!(
            ContentRevision::from_bytes(b"[daemon]"),
            ContentRevision::from_bytes(b"[daemon]\n"),
        );
        assert_ne!(ContentRevision::missing(), ContentRevision::from_bytes(b""),);
    }

    #[test]
    fn reload_receipt_has_the_causal_public_shape() {
        let revision = RuntimeRevision {
            config: ContentRevision::from_bytes(b"config"),
            credentials: ContentRevision::from_bytes(b"credentials"),
        };
        let receipt = ReloadReceipt {
            request_ids: vec![41, 42],
            sources: vec![
                ReloadSource::Watcher,
                ReloadSource::Control,
                ReloadSource::WebApply,
                ReloadSource::Ipc,
                ReloadSource::Signal,
            ],
            requested_revision: revision.clone(),
            applied_revision: revision,
            generation: GenerationId(7),
            outcome: crate::reload::ReloadOutcome::Reloaded,
            coalesced: true,
        };

        assert_eq!(receipt.generation, GenerationId(7));
    }

    #[test]
    fn manual_phase_observations_have_no_rule_identity() {
        let observation = DaemonObservation::DisplayPhaseChanged {
            generation: GenerationId(7),
            rule_id: None,
            display_id: DisplayId("manual".into()),
            old_phase: Phase::Active,
            new_phase: Phase::Blanking,
        };

        assert!(matches!(
            observation,
            DaemonObservation::DisplayPhaseChanged { rule_id: None, .. }
        ));
    }

    #[tokio::test]
    async fn independent_observation_hubs_do_not_cross_talk() {
        let first = ObservationHub::new(1);
        let second = ObservationHub::new(1);
        let mut first_rx = first.subscribe();
        let mut second_rx = second.subscribe();

        first.emit(DaemonObservation::WatchdogPing {
            generation: GenerationId(3),
        });

        assert!(matches!(
            first_rx.recv().await,
            Ok(DaemonObservation::WatchdogPing {
                generation: GenerationId(3)
            })
        ));
        assert!(matches!(
            second_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
