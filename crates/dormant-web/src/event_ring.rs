//! Bounded, in-memory ring buffer for recent `DaemonEvent`s.
//!
//! Used by `GET /api/events/recent` to seed the Events view with history
//! after a page reload.  Lost on daemon restart — no persistent storage.
//!
//! The ring never retains `DaemonEvent::Subscribed` sentinels (they are
//! per-connection markers, not real events).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::PoisonError;

use dormant_core::rules::{ControlMsg, DaemonEvent};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Maximum number of recent events the ring retains.
pub const EVENT_RING_CAP: usize = 500;

/// A single event in the recent-history ring, timestamped at capture.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecentEvent {
    /// Milliseconds since Unix epoch at capture time.
    pub at_epoch_ms: u64,
    /// The daemon event.
    #[serde(flatten)]
    pub event: DaemonEvent,
}

/// Bounded ring of recent [`DaemonEvent`]s, oldest first.
///
/// Thread-safe: [`EventRing::push`] locks internally.
#[derive(Debug)]
pub struct EventRing {
    buf: std::sync::Mutex<VecDeque<RecentEvent>>,
    cap: usize,
}

impl EventRing {
    /// Create a new ring with the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(EVENT_RING_CAP)
    }

    /// Create a new ring with the given capacity.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: std::sync::Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    /// Push an event into the ring, dropping the oldest event if at capacity.
    /// `Subscribed` sentinels are discarded silently.
    pub fn push(&self, event: DaemonEvent) {
        if matches!(event, DaemonEvent::Subscribed) {
            return;
        }
        let at_epoch_ms = epoch_ms();
        let mut buf = self.buf.lock().unwrap_or_else(PoisonError::into_inner);
        if buf.len() >= self.cap {
            buf.pop_front();
        }
        buf.push_back(RecentEvent { at_epoch_ms, event });
    }

    /// Return a snapshot of all events currently in the ring, oldest first,
    /// up to `limit`.  `limit` is clamped to the ring's capacity.
    #[must_use]
    pub fn snapshot(&self, limit: usize) -> Vec<RecentEvent> {
        let buf = self.buf.lock().unwrap_or_else(PoisonError::into_inner);
        let limit = limit.min(self.cap);
        buf.iter().take(limit).cloned().collect()
    }

    /// Number of events currently in the ring.
    #[must_use]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buf
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

fn epoch_ms() -> u64 {
    use std::time::SystemTime;
    let ms: u128 = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(ms).unwrap_or(u64::MAX)
}

/// Background task: subscribe to the engine event broadcast via `ctl_tx` and
/// feed every event into the ring buffer.  On broadcast close (reload),
/// resubscribe.  Never returns unless the control channel closes.
pub(crate) async fn feed_ring(ring: Arc<EventRing>, ctl_tx: mpsc::Sender<ControlMsg>) {
    loop {
        let Ok(mut rx) = subscribe_events(&ctl_tx).await else {
            // Engine control channel closed — daemon is shutting down.
            return;
        };

        loop {
            match rx.recv().await {
                Ok(ev) => ring.push(ev),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Lagged — skip lost events; the ring best-effort.
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Broadcast closed (reload) — resubscribe.
                    break;
                }
            }
        }
    }
}

async fn subscribe_events(
    ctl_tx: &mpsc::Sender<ControlMsg>,
) -> Result<broadcast::Receiver<DaemonEvent>, ()> {
    let (tx, rx) = oneshot::channel();
    ctl_tx
        .send(ControlMsg::SubscribeEvents(tx))
        .await
        .map_err(|_| ())?;
    rx.await.map_err(|_| ())
}
