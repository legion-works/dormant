//! Integration tests for the MQTT sensor source.
//!
//! Require a running broker. `DORMANT_TEST_MQTT=1` enables the ignored tests;
//! `DORMANT_TEST_MQTT_PORT` selects its port (default `1883`). Sources wait for
//! matching `SubAck` packets before a test publishes an asserted state. Publishers
//! wait for `ConnAck` and the matching `QoS` 1 `PubAck`, including retained cleanup.
//! Retained tests clear their broker state before making their final assertion.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dormant_core::config::schema::{MqttSensorCfg, SensorKind};
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState};
use dormant_sensors::mqtt::{MqttLifecycle, MqttSource};
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Outgoing, Packet, QoS};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// SNZB-06P occupancy fixture — `{"occupancy":true,"illuminance":12,"linkquality":120}`.
const SNZB06P_FIXTURE: &[u8] = include_bytes!("../fixtures/z2m_snzb06p.json");

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn mqtt_port() -> Option<u16> {
    if std::env::var("DORMANT_TEST_MQTT").as_deref() != Ok("1") {
        eprintln!("skipping mqtt integration test (DORMANT_TEST_MQTT != 1)");
        return None;
    }

    Some(
        std::env::var("DORMANT_TEST_MQTT_PORT")
            .map_or(Ok(1883), |value| value.parse())
            .expect("DORMANT_TEST_MQTT_PORT must be a valid u16"),
    )
}

fn unique_tag() -> String {
    let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the epoch")
        .as_nanos();
    format!("{}-{counter}-{nanos}", std::process::id())
}

fn mqtt_topic(kind: &str) -> String {
    format!("test/dormant-{kind}-{}", unique_tag())
}

fn broker_url(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

fn mqtt_cfg(topic: String, port: u16) -> MqttSensorCfg {
    MqttSensorCfg {
        broker_url: broker_url(port),
        topic,
        field: "/occupancy".into(),
        payload_on: None,
        payload_off: None,
        kind: SensorKind::Presence,
        hold_time: None,
        stale_timeout: None,
        availability_topic: None,
        availability_payload_online: "online".into(),
        availability_payload_offline: "offline".into(),
    }
}

async fn wait_for_subscribed(rx: &mut mpsc::UnboundedReceiver<MqttLifecycle>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut connected = false;
        while let Some(lifecycle) = rx.recv().await {
            match lifecycle {
                MqttLifecycle::Connected => connected = true,
                MqttLifecycle::Subscribed => {
                    assert!(connected, "Subscribed must follow ConnAck");
                    return;
                }
            }
        }
        panic!("source lifecycle channel closed before Subscribed");
    })
    .await
    .expect("source should receive matching SubAck packets for every topic");
}

async fn start_source(
    port: u16,
    sensor_id: SensorId,
    topic: String,
) -> (
    mpsc::Receiver<PresenceEvent>,
    CancellationToken,
    JoinHandle<()>,
) {
    let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
    let source = MqttSource::new(
        broker_url(port),
        vec![(sensor_id, mqtt_cfg(topic, port))],
        None,
    )
    .with_lifecycle_sender(lifecycle_tx);
    let (tx, rx) = mpsc::channel(16);
    let (ctl_tx, _ctl_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        let _ = Box::new(source).run(tx, ctl_tx, cancel_clone).await;
    });

    wait_for_subscribed(&mut lifecycle_rx).await;
    (rx, cancel, handle)
}

async fn await_connack(eventloop: &mut EventLoop) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::Incoming(Packet::ConnAck(_)) = eventloop
                .poll()
                .await
                .expect("publisher event loop should remain connected")
            {
                return;
            }
        }
    })
    .await
    .expect("publisher should receive ConnAck");
}

async fn publisher(port: u16) -> (AsyncClient, EventLoop) {
    let mut options = MqttOptions::new(
        format!("dormant-integration-publisher-{}", unique_tag()),
        "127.0.0.1",
        port,
    );
    options.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(options, 16);
    await_connack(&mut eventloop).await;
    (client, eventloop)
}

async fn publish_qos1_and_wait(
    client: &AsyncClient,
    eventloop: &mut EventLoop,
    topic: &str,
    retain: bool,
    payload: impl Into<Vec<u8>>,
) {
    client
        .publish(topic, QoS::AtLeastOnce, retain, payload)
        .await
        .expect("publish should enqueue");

    tokio::time::timeout(Duration::from_secs(5), async {
        let mut publish_id = None;
        loop {
            match eventloop
                .poll()
                .await
                .expect("publisher event loop should remain connected")
            {
                Event::Outgoing(Outgoing::Publish(pkid)) => publish_id = Some(pkid),
                Event::Incoming(Packet::PubAck(puback)) if Some(puback.pkid) == publish_id => {
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("publisher should receive the matching PubAck");
}

async fn stop_source(cancel: CancellationToken, handle: JoinHandle<()>) {
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("source should stop after cancellation")
        .expect("source task should not panic");
}

#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_round_trip_publishes_presence_event() {
    let Some(port) = mqtt_port() else {
        return;
    };
    let topic = mqtt_topic("round-trip");
    let (mut rx, cancel, handle) = start_source(
        port,
        SensorId("integration-round-trip".into()),
        topic.clone(),
    )
    .await;
    let (publisher, mut eventloop) = publisher(port).await;

    publish_qos1_and_wait(
        &publisher,
        &mut eventloop,
        &topic,
        false,
        br#"{"occupancy":true,"illuminance":12}"#.to_vec(),
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("should receive PresenceEvent")
        .expect("source event channel should remain open");
    assert_eq!(event.state, SensorState::Present);
    assert!((event.confidence - 1.0).abs() < f32::EPSILON);

    stop_source(cancel, handle).await;
}

#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_retained_state_delivered_on_subscribe() {
    let Some(port) = mqtt_port() else {
        return;
    };
    let topic = mqtt_topic("retained-state");
    let (publisher, mut eventloop) = publisher(port).await;

    publish_qos1_and_wait(
        &publisher,
        &mut eventloop,
        &topic,
        true,
        SNZB06P_FIXTURE.to_vec(),
    )
    .await;

    let (mut rx, cancel, handle) =
        start_source(port, SensorId("retained-state-test".into()), topic.clone()).await;
    let outcome: Result<(), String> = async {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .map_err(|_| "timed out waiting for the retained PresenceEvent".to_string())?
            .ok_or_else(|| "channel closed before an event arrived".to_string())?;
        if event.state == SensorState::Present {
            Ok(())
        } else {
            Err(format!(
                "expected Present from retained delivery, got {:?}",
                event.state
            ))
        }
    }
    .await;

    stop_source(cancel, handle).await;
    publish_qos1_and_wait(&publisher, &mut eventloop, &topic, true, Vec::new()).await;
    assert!(
        outcome.is_ok(),
        "retained state delivery failed: {}",
        outcome.err().unwrap_or_default()
    );
}

#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_retained_availability_offline_on_subscribe() {
    let Some(port) = mqtt_port() else {
        return;
    };
    let topic = mqtt_topic("retained-availability");
    let availability_topic = dormant_sensors::mqtt::availability_topic(&topic);
    let (publisher, mut eventloop) = publisher(port).await;

    publish_qos1_and_wait(
        &publisher,
        &mut eventloop,
        &availability_topic,
        true,
        b"offline".to_vec(),
    )
    .await;

    let (mut rx, cancel, handle) = start_source(
        port,
        SensorId("retained-availability-test".into()),
        topic.clone(),
    )
    .await;
    let outcome: Result<(), String> = async {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .map_err(|_| "timed out waiting for retained Unavailable event".to_string())?
            .ok_or_else(|| "channel closed before an event arrived".to_string())?;
        if event.state == SensorState::Unavailable {
            Ok(())
        } else {
            Err(format!(
                "expected Unavailable from retained offline delivery, got {:?}",
                event.state
            ))
        }
    }
    .await;

    stop_source(cancel, handle).await;
    publish_qos1_and_wait(
        &publisher,
        &mut eventloop,
        &availability_topic,
        true,
        Vec::new(),
    )
    .await;
    assert!(
        outcome.is_ok(),
        "retained availability delivery failed: {}",
        outcome.err().unwrap_or_default()
    );
}

#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_concurrent_sources_receive_only_their_own_state() {
    let Some(port) = mqtt_port() else {
        return;
    };
    let topic_a = mqtt_topic("concurrent-a");
    let topic_b = mqtt_topic("concurrent-b");
    let (mut rx_a, cancel_a, handle_a) =
        start_source(port, SensorId("concurrent-a".into()), topic_a.clone()).await;
    let (mut rx_b, cancel_b, handle_b) =
        start_source(port, SensorId("concurrent-b".into()), topic_b.clone()).await;
    let (publisher, mut eventloop) = publisher(port).await;

    publish_qos1_and_wait(
        &publisher,
        &mut eventloop,
        &topic_a,
        false,
        br#"{"occupancy":true}"#.to_vec(),
    )
    .await;
    let first = tokio::time::timeout(Duration::from_secs(5), rx_a.recv())
        .await
        .expect("first source should receive its state")
        .expect("first source channel should remain open");
    assert_eq!(first.state, SensorState::Present);
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx_b.recv())
            .await
            .is_err(),
        "second source received the first source's state"
    );

    let availability_b = dormant_sensors::mqtt::availability_topic(&topic_b);
    publish_qos1_and_wait(
        &publisher,
        &mut eventloop,
        &availability_b,
        false,
        b"offline".to_vec(),
    )
    .await;
    let second = tokio::time::timeout(Duration::from_secs(5), rx_b.recv())
        .await
        .expect("second source should receive its availability state")
        .expect("second source channel should remain open");
    assert_eq!(second.state, SensorState::Unavailable);
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx_a.recv())
            .await
            .is_err(),
        "first source received the second source's availability state"
    );

    stop_source(cancel_a, handle_a).await;
    stop_source(cancel_b, handle_b).await;
}

// ── Reconnect tests ───────────────────────────────────────────────────────────────

/// Issue #213 — verify that MQTT subscriptions are issued exactly once per
/// `ConnAck`, for both initial connection and reconnect.
///
/// ## Bug anatomy (before fix)
///
/// `connect()` called `subscribe_topics()` immediately when constructing the
/// client/eventloop — **before** any `ConnAck` arrived. The `ConnAck` handler
/// then did:
/// - initial `ConnAck`: set `initial_connack_seen = true`, no subscribe
/// - reconnect `ConnAck`: called `subscribe_topics()` again
///
/// Result: **N subs from `connect()` + N subs from reconnect = 2N** for a
/// single reconnect cycle, and the `Subscribed` readiness event could fire
/// against a stale acknowledgement counter.
///
/// ## Fix
///
/// `connect()` now only constructs the `AsyncClient`/`EventLoop` pair.
/// Every `ConnAck` is the sole subscription site (initial and reconnect
/// identical). Counters are reset before each batch, so `Subscribed` fires
/// exactly when `acknowledged == queued`.
///
/// ## Test approach
///
/// We drive the source through two connection cycles and verify that
/// `Subscribed` fires exactly once per cycle:
///
/// 1. Spawn source → wait for initial `Subscribed` (N topics, 1 event)
/// 2. Cancel source task → restart fresh → wait for reconnect `Subscribed`
/// 3. Assert: `Subscribed` fires exactly twice (one per `ConnAck`)
///
/// The acked-vs-queued invariant is guaranteed by the counter reset before
/// each subscription batch: `pending_subacks`, `acknowledged_subscriptions`,
/// and `outgoing_subscriptions` are all cleared to 0 on every `ConnAck`,
/// and `queued_subscriptions` is set after `subscribe_topics()` returns.
/// Since `acknowledged` only increments after looking up the pkid in
/// `pending_subacks`, it can never exceed `queued`.
///
/// The `#[ignore]` attribute mirrors other broker-dependent tests;
/// enable with `DORMANT_TEST_MQTT=1`.
#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_reconnect() {
    let Some(port) = mqtt_port() else {
        return;
    };

    let topic_a = mqtt_topic("reconnect-a");
    let topic_b = mqtt_topic("reconnect-b");
    let topics = [topic_a.clone(), topic_b.clone()];

    let sensors: Vec<_> = topics
        .iter()
        .enumerate()
        .map(|(i, t)| (SensorId(format!("sensor-{i}")), mqtt_cfg(t.clone(), port)))
        .collect();

    // Lifecycle channel: tracks Connected + Subscribed per cycle.
    let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();

    // ── Phase 1: initial connection ─────────────────────────────────────────
    let source = MqttSource::new(broker_url(port), sensors.clone(), None)
        .with_lifecycle_sender(lifecycle_tx.clone());

    let (tx, _rx) = mpsc::channel(16);
    let (ctl_tx, _ctl_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move {
        let _ = Box::new(source).run(tx, ctl_tx, cancel_clone).await;
    });

    // Wait for initial Subscribed.
    wait_for_subscribed(&mut lifecycle_rx).await;

    // Collect any other lifecycle events that arrived.
    let mut subscribed_events = 0usize;
    while let Some(l) = lifecycle_rx.recv().await {
        if l == MqttLifecycle::Subscribed {
            subscribed_events += 1;
        }
    }
    assert_eq!(
        subscribed_events, 1,
        "initial connection should produce exactly 1 Subscribed event, got {subscribed_events}"
    );

    // ── Phase 2: reconnect ─────────────────────────────────────────────────
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("first source run should exit after cancellation");

    // Drain any lingering lifecycle events from the first run.
    let _ = tokio::time::timeout(Duration::from_millis(100), async {
        while lifecycle_rx.recv().await.is_some() {}
    })
    .await;

    // Restart source for the reconnect cycle.
    let source2 = MqttSource::new(broker_url(port), sensors, None)
        .with_lifecycle_sender(lifecycle_tx.clone());

    let (tx2, _rx) = mpsc::channel(16);
    let (ctl_tx2, _ctl_rx) = mpsc::channel(8);
    let cancel2 = CancellationToken::new();
    let cancel2_inner = cancel2.clone();

    let handle2 = tokio::spawn(async move {
        let _ = Box::new(source2).run(tx2, ctl_tx2, cancel2_inner).await;
    });

    // Wait for reconnect Subscribed.
    wait_for_subscribed(&mut lifecycle_rx).await;

    // Collect all remaining Subscribed events from the reconnect cycle.
    let mut reconnect_subscribed = 1usize; // counted the reconnect one above
    while let Some(l) = lifecycle_rx.recv().await {
        if l == MqttLifecycle::Subscribed {
            reconnect_subscribed += 1;
        }
    }

    // ── Assertions ─────────────────────────────────────────────────────────
    // With the fix: each connection cycle issues exactly N subscriptions and
    // fires exactly one `Subscribed` event. Two cycles = 2 events total.
    // Without the fix (bug #213): the initial cycle also subscribes in
    // `connect()`, producing two `Subscribed` events for the first cycle
    // (one for the connect()-batch, one for the ConnAck-batch), so we'd
    // see 3 events across two cycles instead of 2.
    let total_subscribed = subscribed_events + reconnect_subscribed;
    assert_eq!(
        total_subscribed, 2,
        "two connection cycles should produce exactly 2 Subscribed events, got {total_subscribed}"
    );

    // ── Cleanup ─────────────────────────────────────────────────────────────
    cancel2.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle2).await;
}
