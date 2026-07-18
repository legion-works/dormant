//! Daemon reload coordination types shared by `dormantd` and `dormant-web`.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::{mpsc, oneshot};

use crate::observation::{DaemonObservation, ObservationHub, ReloadReceipt, ReloadSource};

/// Outcome of a reload attempt, published on the daemon-level reload bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The new config was applied.
    Reloaded,
    /// The reload was rejected; the old config remains active. Carries a
    /// human-readable detail.
    Rejected(String),
}

/// A reload request awaiting processing by the daemon-owned coordinator.
#[derive(Debug)]
pub struct ReloadRequest {
    /// Identity used to correlate the request with its eventual receipt.
    pub request_id: u64,
    /// Surface that initiated the reload.
    pub source: ReloadSource,
    /// Optional private completion path for the originating requester.
    pub receipt_tx: Option<oneshot::Sender<ReloadReceipt>>,
}

/// Cloneable producer for causally-correlated reload requests.
#[derive(Clone, Debug)]
pub struct ReloadRequester {
    tx: mpsc::Sender<ReloadRequest>,
    next_request_id: Arc<AtomicU64>,
    observations: ObservationHub,
}

impl ReloadRequester {
    /// Create producers that feed `tx` and share one request-id sequence.
    #[must_use]
    pub fn new(tx: mpsc::Sender<ReloadRequest>) -> Self {
        Self {
            tx,
            next_request_id: Arc::new(AtomicU64::new(1)),
            observations: ObservationHub::new(64),
        }
    }

    /// Enqueue a request and return its private completion receiver.
    pub async fn request(
        &self,
        source: ReloadSource,
    ) -> Option<(u64, oneshot::Receiver<ReloadReceipt>)> {
        let request_id = self.allocate_id();
        let (receipt_tx, receipt_rx) = oneshot::channel();
        self.tx
            .send(ReloadRequest {
                request_id,
                source,
                receipt_tx: Some(receipt_tx),
            })
            .await
            .ok()?;
        Some((request_id, receipt_rx))
    }

    /// Enqueue a request whose caller does not need the receipt.
    pub async fn notify(&self, source: ReloadSource) -> bool {
        self.tx
            .send(ReloadRequest {
                request_id: self.allocate_id(),
                source,
                receipt_tx: None,
            })
            .await
            .is_ok()
    }

    /// Subscribe to daemon-owned causal observations for these requests.
    #[must_use]
    pub fn subscribe_observations(&self) -> tokio::sync::broadcast::Receiver<DaemonObservation> {
        self.observations.subscribe()
    }

    /// Return the causal observation hub shared with the coordinator.
    #[must_use]
    pub fn observations(&self) -> ObservationHub {
        self.observations.clone()
    }

    fn allocate_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }
}
