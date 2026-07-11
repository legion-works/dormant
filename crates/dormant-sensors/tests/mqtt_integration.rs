//! Integration test for the MQTT sensor source.
//!
//! Requires a running MQTT broker on `127.0.0.1:1883`.  Set
//! `DORMANT_TEST_MQTT=1` to run (the test is `#[ignore]`d by default).
//!
//! Belt+braces: both `#[ignore]` and an early-return env check guard against
//! accidental CI runs without a broker.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dormant_core::config::schema::{MqttSensorCfg, SensorKind};
use dormant_core::traits::SensorSource;
use dormant_core::types::{SensorId, SensorState};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// SNZB-06P occupancy fixture — `{"occupancy":true,"illuminance":12,"linkquality":120}`.
const SNZB06P_FIXTURE: &[u8] = include_bytes!("../fixtures/z2m_snzb06p.json");

/// A per-process-invocation-unique suffix beyond `std::process::id()` (spec
/// F14): the PID alone can collide across separate `cargo test` invocations
/// on a broker that persisted a retained value from a prior, differently
/// aborted run reusing the same PID. Nanosecond wall-clock time closes that
/// gap without adding a dependency.
fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the epoch")
        .as_nanos()
}

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
        availability_topic: None,
        availability_payload_online: "online".into(),
        availability_payload_offline: "offline".into(),
    };

    let source = dormant_sensors::mqtt::MqttSource::new(
        "127.0.0.1:1883".into(),
        vec![(sensor_id, cfg)],
        None,
    );

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

/// Prove that a value **retained broker-side before the daemon ever
/// subscribes** is delivered on subscribe, exactly as it would be after a
/// daemon restart against a broker with an already-retained occupancy state
/// (spec §3.1 / §8).
///
/// Ordering is the whole point of this test: publish (retained) → drain the
/// publisher's event loop so the broker actually holds it (P3 — `publish()`
/// only enqueues; rumqttc only flushes to the wire as the event loop is
/// polled, mirroring the drain above at `mqtt_integration.rs:97-100`) →
/// *then* construct and connect `MqttSource`. No publish happens after the
/// source subscribes — delivery must come from the broker's retained store.
///
/// Cleanup shape (P7/P9 — pinned, no `Drop` guard, no `block_on`): `Drop` is
/// sync and can't `.await`, and `block_on`/`block_in_place` panic on the
/// current-thread `#[tokio::test]` runtime the rest of this file already
/// runs on. So the arrival check is PANIC-FREE by construction — it produces
/// a `Result<(), String>` (timeout or wrong-event → `Err`, no `assert!`/
/// `unwrap` in that path) — the retained-clear (empty-payload retained
/// publish + drain) always runs after it regardless of outcome, and exactly
/// one `assert!` fires at the very end, once cleanup is behind us.
#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_retained_state_delivered_on_subscribe() {
    if std::env::var("DORMANT_TEST_MQTT").as_deref() != Ok("1") {
        eprintln!("skipping mqtt integration test (DORMANT_TEST_MQTT != 1)");
        return;
    }

    let topic = format!(
        "test/dormant-retained-{}-{}-state",
        std::process::id(),
        unique_suffix()
    );

    // ── Publisher client, used both for the retained seed and the cleanup ──
    let mut pub_opts = MqttOptions::new("dormant-retained-state-publisher", "127.0.0.1", 1883);
    pub_opts.set_clean_session(true);
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_opts, 100);

    // Wait for the publisher to connect.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Seed the retained value BEFORE the MqttSource ever subscribes ──────
    pub_client
        .publish(&topic, QoS::AtLeastOnce, true, SNZB06P_FIXTURE)
        .await
        .expect("retained publish should succeed");

    // Drain so the retained publish is actually held broker-side (P3).
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while (pub_eventloop.poll().await).is_ok() {}
    })
    .await;

    // ── Construct + connect the MqttSource AFTER the retain is in place ────
    let sensor_id = SensorId("retained-state-test".into());
    let cfg = MqttSensorCfg {
        broker_url: "127.0.0.1:1883".into(),
        topic: topic.clone(),
        field: "/occupancy".into(),
        payload_on: None,
        payload_off: None,
        kind: SensorKind::Presence,
        hold_time: None,
        stale_timeout: None,
        availability_topic: None,
        availability_payload_online: "online".into(),
        availability_payload_offline: "offline".into(),
    };

    let source = dormant_sensors::mqtt::MqttSource::new(
        "127.0.0.1:1883".into(),
        vec![(sensor_id, cfg)],
        None,
    );

    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move {
        let _ = Box::new(source).run(tx, cancel_clone).await;
    });

    // ── Arrival check — PANIC-FREE by construction (Result, not assert!/
    // unwrap) so a failure here cannot skip the cleanup below. NO publish
    // happens between source construction and this check — the event, if
    // any, can only have come from the broker's retained store.
    let outcome: Result<(), String> = async {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .map_err(|_| "timed out waiting for the retained PresenceEvent".to_string())?
            .ok_or_else(|| "channel closed before an event arrived".to_string())?;

        if event.state != SensorState::Present {
            return Err(format!(
                "expected Present from retained delivery, got {:?}",
                event.state
            ));
        }
        Ok(())
    }
    .await;

    // ── Cleanup — ALWAYS runs, regardless of `outcome` ──────────────────────
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    // Clear the retained value (empty-payload retained publish is the MQTT
    // convention for deleting a retained message) so this test can't poison
    // a later run reusing the topic (spec F14).
    let _ = pub_client
        .publish(&topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while (pub_eventloop.poll().await).is_ok() {}
    })
    .await;

    // ── Single assert, AFTER cleanup ────────────────────────────────────────
    assert!(
        outcome.is_ok(),
        "retained state delivery failed: {}",
        outcome.err().unwrap_or_default()
    );
}

/// Sibling of [`mqtt_retained_state_delivered_on_subscribe`]: a retained
/// `"offline"` on the (derived) availability topic, seeded before the
/// `MqttSource` ever subscribes, must surface as `SensorState::Unavailable`
/// on subscribe — the fail-safe path documented in `mqtt.rs`'s module docs
/// (a broker replaying a stale retained "offline" resets the stale-sensor
/// sweep's clock exactly as a fresh publish would).
///
/// Same ordering discipline (publish retained → drain → THEN construct
/// `MqttSource`) and the same panic-free-body + cleanup-then-assert shape as
/// the sibling test above — see its doc comment for the full P3/P7/P9
/// rationale.
#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[tokio::test]
async fn mqtt_retained_availability_offline_on_subscribe() {
    if std::env::var("DORMANT_TEST_MQTT").as_deref() != Ok("1") {
        eprintln!("skipping mqtt integration test (DORMANT_TEST_MQTT != 1)");
        return;
    }

    let topic = format!(
        "test/dormant-retained-{}-{}-avail",
        std::process::id(),
        unique_suffix()
    );
    let avail_topic = dormant_sensors::mqtt::availability_topic(&topic);

    // ── Publisher client, used both for the retained seed and the cleanup ──
    let mut pub_opts = MqttOptions::new("dormant-retained-avail-publisher", "127.0.0.1", 1883);
    pub_opts.set_clean_session(true);
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_opts, 100);

    // Wait for the publisher to connect.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Seed the retained "offline" BEFORE the MqttSource ever subscribes ──
    pub_client
        .publish(&avail_topic, QoS::AtLeastOnce, true, b"offline".to_vec())
        .await
        .expect("retained publish should succeed");

    // Drain so the retained publish is actually held broker-side (P3).
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while (pub_eventloop.poll().await).is_ok() {}
    })
    .await;

    // ── Construct + connect the MqttSource AFTER the retain is in place ────
    let sensor_id = SensorId("retained-availability-test".into());
    let cfg = MqttSensorCfg {
        broker_url: "127.0.0.1:1883".into(),
        topic: topic.clone(),
        field: "/occupancy".into(),
        payload_on: None,
        payload_off: None,
        kind: SensorKind::Presence,
        hold_time: None,
        stale_timeout: None,
        availability_topic: None,
        availability_payload_online: "online".into(),
        availability_payload_offline: "offline".into(),
    };

    let source = dormant_sensors::mqtt::MqttSource::new(
        "127.0.0.1:1883".into(),
        vec![(sensor_id, cfg)],
        None,
    );

    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move {
        let _ = Box::new(source).run(tx, cancel_clone).await;
    });

    // ── Arrival check — PANIC-FREE by construction (Result, not assert!/
    // unwrap). NO publish happens after source construction — the event, if
    // any, can only have come from the broker's retained store.
    let outcome: Result<(), String> = async {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .map_err(|_| "timed out waiting for the retained Unavailable event".to_string())?
            .ok_or_else(|| "channel closed before an event arrived".to_string())?;

        if event.state != SensorState::Unavailable {
            return Err(format!(
                "expected Unavailable from retained offline delivery, got {:?}",
                event.state
            ));
        }
        Ok(())
    }
    .await;

    // ── Cleanup — ALWAYS runs, regardless of `outcome` ──────────────────────
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    // Clear the retained value so this test can't poison a later run reusing
    // the topic (spec F14).
    let _ = pub_client
        .publish(&avail_topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while (pub_eventloop.poll().await).is_ok() {}
    })
    .await;

    // ── Single assert, AFTER cleanup ────────────────────────────────────────
    assert!(
        outcome.is_ok(),
        "retained availability-offline delivery failed: {}",
        outcome.err().unwrap_or_default()
    );
}
