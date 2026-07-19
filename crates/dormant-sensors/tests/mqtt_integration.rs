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
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        let _ = Box::new(source).run(tx, cancel_clone).await;
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
