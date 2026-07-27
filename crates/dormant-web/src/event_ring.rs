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
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The broadcast overran — events were lost.
                    // The WS handler emits a `stream_lagged` frame to the browser;
                    // the ring has no equivalent wire event, so we log the gap.
                    // Endpoint consumers can inspect the `Lagged` counter over time
                    // but individual lost events are unrecoverable.
                    tracing::warn!(skipped = n, "event_ring lagged — {} events lost", n);
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

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::types::SensorId;

    fn sensor_event(name: &str) -> DaemonEvent {
        DaemonEvent::SensorChanged {
            sensor: SensorId(name.to_string()),
            state: dormant_core::types::SensorState::Present,
        }
    }

    #[test]
    fn push_retains_oldest_first_order() {
        let ring = EventRing::with_capacity(10);
        ring.push(sensor_event("s1"));
        ring.push(sensor_event("s2"));
        ring.push(sensor_event("s3"));

        let snap = ring.snapshot(10);
        assert_eq!(snap.len(), 3);
        // Oldest first: s1, s2, s3
        assert!(
            matches!(&snap[0].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s1")
        );
        assert!(
            matches!(&snap[1].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s2")
        );
        assert!(
            matches!(&snap[2].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s3")
        );
    }

    #[test]
    fn push_oldest_out_at_capacity() {
        let ring = EventRing::with_capacity(3);
        ring.push(sensor_event("s1"));
        ring.push(sensor_event("s2"));
        ring.push(sensor_event("s3"));
        ring.push(sensor_event("s4")); // Should evict s1

        let snap = ring.snapshot(10);
        assert_eq!(snap.len(), 3);
        assert!(
            matches!(&snap[0].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s2")
        );
        assert!(
            matches!(&snap[1].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s3")
        );
        assert!(
            matches!(&snap[2].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s4")
        );
    }

    #[test]
    fn push_discards_subscribed_sentinel() {
        let ring = EventRing::new();
        ring.push(sensor_event("s1"));
        ring.push(DaemonEvent::Subscribed);
        ring.push(sensor_event("s2"));

        let snap = ring.snapshot(10);
        assert_eq!(snap.len(), 2);
        assert!(
            matches!(&snap[0].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s1")
        );
        assert!(
            matches!(&snap[1].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s2")
        );
    }

    #[test]
    fn snapshot_limit_less_than_contents() {
        let ring = EventRing::new();
        for i in 0..5 {
            ring.push(sensor_event(&format!("s{i}")));
        }
        let snap = ring.snapshot(2);
        assert_eq!(snap.len(), 2);
        assert!(
            matches!(&snap[0].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s0")
        );
        assert!(
            matches!(&snap[1].event, DaemonEvent::SensorChanged { sensor, .. } if sensor.0 == "s1")
        );
    }

    #[test]
    fn snapshot_limit_greater_than_cap() {
        let ring = EventRing::with_capacity(3);
        ring.push(sensor_event("s1"));
        ring.push(sensor_event("s2"));
        // limit 500 > cap 3 → clamped to cap
        let snap = ring.snapshot(500);
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn snapshot_limit_zero_is_empty() {
        let ring = EventRing::new();
        ring.push(sensor_event("s1"));
        let snap = ring.snapshot(0);
        assert!(snap.is_empty());
    }

    #[test]
    fn len_reports_correct_count() {
        let ring = EventRing::new();
        assert_eq!(ring.len(), 0);
        ring.push(sensor_event("s1"));
        assert_eq!(ring.len(), 1);
        ring.push(DaemonEvent::Subscribed);
        assert_eq!(ring.len(), 1, "Subscribed must not increment len");
    }

    /// Verifies that the `feed_ring` task resubscribes after the broadcast
    /// closes (generation switch).  Mirrors the pattern used by the WS bridge's
    /// `reload_resubscribe_keeps_streaming_and_emits_config_reloaded` test.
    #[tokio::test]
    async fn feed_ring_resubscribes_on_closed() {
        let ring = Arc::new(EventRing::with_capacity(64));
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
        let (gen1_tx, gen1_rx) = broadcast::channel::<DaemonEvent>(64);
        let (gen2_tx, gen2_rx) = broadcast::channel::<DaemonEvent>(64);
        // Keep receivers alive so broadcasts don't drop.
        let _gen1_rx = gen1_rx;
        let _gen2_rx = gen2_rx;

        // Engine: serves gen1 subscription first, then gen2 after gen1 is dropped.
        let gen1_tx_raw = gen1_tx.clone();
        let gen2_tx_raw = gen2_tx.clone();
        tokio::spawn(async move {
            // First subscribe — return gen1 receiver, then drop our clone
            // so the broadcast closes when the test drops gen1_tx.
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::SubscribeEvents(tx) = msg {
                    let _ = tx.send(gen1_tx_raw.subscribe());
                    break;
                }
            }
            drop(gen1_tx_raw);
            // Wait for second subscribe (after gen1 closed) — return gen2 receiver.
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::SubscribeEvents(tx) = msg {
                    let _ = tx.send(gen2_tx_raw.subscribe());
                    break;
                }
            }
        });

        // Spawn the feeder.
        let ring_clone = ring.clone();
        let ctl_clone = ctl_tx.clone();
        tokio::spawn(async move {
            feed_ring(ring_clone, ctl_clone).await;
        });

        // Let feeder subscribe and process gen1 events.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send gen1 events.
        let _ = gen1_tx.send(sensor_event("gen1-a"));
        let _ = gen1_tx.send(sensor_event("gen1-b"));

        // Wait for gen1 events to land in the ring.
        for _ in 0..100 {
            if ring.len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(ring.len(), 2, "gen1 events should be in the ring");

        // Close gen1 broadcast (simulate generation switch).
        drop(gen1_tx);

        // Give feeder time to detect Closed and resubscribe to gen2.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Send gen2 events.
        let _ = gen2_tx.send(sensor_event("gen2-a"));

        // Wait for gen2 event to land after resubscribe.
        for _ in 0..100 {
            if ring.len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Both gen1 and gen2 events should be present.
        let snap = ring.snapshot(100);
        let names: Vec<String> = snap
            .iter()
            .filter_map(|re| {
                if let DaemonEvent::SensorChanged { sensor, .. } = &re.event {
                    Some(sensor.0.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            names.contains(&"gen1-a".to_string()),
            "gen1-a should survive resubscribe"
        );
        assert!(
            names.contains(&"gen1-b".to_string()),
            "gen1-b should survive resubscribe"
        );
        assert!(
            names.contains(&"gen2-a".to_string()),
            "gen2-a should arrive after resubscribe"
        );
    }
}
