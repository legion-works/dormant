//! Integration test for the MQTT sensor source.
//!
//! Requires a running MQTT broker on `127.0.0.1:1883`.  Set
//! `DORMANT_TEST_MQTT=1` to run (the test is `#[ignore]`d by default).
//!
//! Belt+braces: both `#[ignore]` and an early-return env check guard against
//! accidental CI runs without a broker.

use std::time::Duration;

use dormant_core::config::schema::{MqttSensorCfg, SensorKind};
use dormant_core::traits::SensorSource;
use dormant_core::types::{SensorId, SensorState};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Full round-trip: connect to a broker, publish a fixture payload to a test
/// topic, and assert that a [`PresenceEvent`] arrives on the channel within 5s.
#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_round_trip_publishes_presence_event() {
    // Belt+braces: env check alongside #[ignore] so a direct `cargo test` run
    // without the env var still skips gracefully.
    if std::env::var("DORMANT_TEST_MQTT").as_deref() != Ok("1") {
        eprintln!("skipping mqtt integration test (DORMANT_TEST_MQTT != 1)");
        return;
    }

    let topic = format!("test/dormant-integration-{}", std::process::id());

    // ── Set up the MqttSource ──────────────────────────────────────────────
    let sensor_id = SensorId("integration-test".into());
    let cfg = MqttSensorCfg {
        broker_url: "127.0.0.1:1883".into(),
        topic: topic.clone(),
        field: "/occupancy".into(),
        payload_on: None,
        payload_off: None,
        kind: SensorKind::Presence,
        hold_time: None,
        stale_timeout: None,
    };

    let source =
        dormant_sensors::mqtt::MqttSource::new("127.0.0.1:1883".into(), vec![(sensor_id, cfg)]);

    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // ── Spawn the source ───────────────────────────────────────────────────
    let handle = tokio::spawn(async move {
        let _ = Box::new(source).run(tx, cancel_clone).await;
    });

    // Give the source time to connect and subscribe.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Publish a fixture payload via a second client ──────────────────────
    let mut mqttopts = MqttOptions::new("dormant-integration-publisher", "127.0.0.1", 1883);
    mqttopts.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(mqttopts, 100);

    // Wait for the publisher to connect.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let payload = br#"{"occupancy":true,"illuminance":12}"#;
    client
        .publish(&topic, QoS::AtLeastOnce, false, payload)
        .await
        .expect("publish should succeed");

    // Drain a few events from the publisher's event loop so the publish
    // actually goes out.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while (eventloop.poll().await).is_ok() {}
    })
    .await;

    // ── Assert the event arrives ───────────────────────────────────────────
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("should receive PresenceEvent within 5s")
        .expect("channel should not be closed");

    assert_eq!(event.state, SensorState::Present);
    assert!((event.confidence - 1.0).abs() < f32::EPSILON);

    // ── Clean up ───────────────────────────────────────────────────────────
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
