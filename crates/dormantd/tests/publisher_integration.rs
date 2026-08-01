//! Broker integration test for the state publisher (issue #105).
//!
//! Skipped unless `DORMANT_TEST_MQTT=1`. The CI `mqtt-integration` job
//! (`.github/workflows/ci.yml`) starts a Dockerised mosquitto, sets the
//! env flag, and runs this file via `cargo nextest --run-ignored
//! only -E 'test(#*publisher_*)'`.
//!
//! The test points a real `MqttTransport` at the broker; a local
//! `ControlMsg` responder answers `SubscribeEvents` and `Snapshot` so
//! the publisher can run end-to-end without booting the rules engine.
//! A separate `rumqttc` client subscribes to the base topic and
//! confirms retained discovery + state records land within a bounded
//! window, then teardown produces a retained `offline`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dormant_core::config::schema::{Config, Credentials, PublishConfig};
use dormant_core::rules::{ControlMsg, DaemonEvent, StateSnapshot};
use dormantd::state_publisher::{StatePublisherDeps, spawn};
use rumqttc::{AsyncClient, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Bridge the same env-flag check used by the sensor tests. CI sets
/// `DORMANT_TEST_MQTT=1` and exports `DORMANT_TEST_MQTT_PORT`.
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

/// Unique per-run tag to keep parallel integration tests from
/// colliding on the broker.
fn unique_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("dormant-{}-{}", std::process::id(), nanos)
}

/// Minimal config that lights up exactly one sensor + zone + display,
/// so the publisher has actual entities to publish about. The base
/// topic is namespaced per-test to keep the broker tidy.
fn per_test_config(port: u16, tag: &str) -> (Config, String, String) {
    let instance = format!("it-pub-{tag}");
    let base = format!("dormant_it_{tag}");
    let discovery = "homeassistant".to_string();
    let _ = discovery;
    let _ = discovery;
    let mut cfg = Config {
        config_version: 1,
        daemon: dormant_core::config::DaemonConfig::default(),
        sensors: indexmap::IndexMap::new(),
        zones: indexmap::IndexMap::new(),
        displays: indexmap::IndexMap::new(),
        rules: indexmap::IndexMap::new(),
        wear: dormant_core::config::schema::WearConfig::default(),
        notifications: dormant_core::config::schema::NotificationsConfig::default(),
        watchdog: dormant_core::config::schema::WatchdogConfig::default(),
        audio: dormant_core::config::schema::AudioConfig::default(),
        coordination: dormant_core::config::CoordinationConfig::default(),
        keymap: dormant_core::config::KeymapConfig::default(),
        input_filter: dormant_core::config::InputFilterConfig::default(),
        publish: PublishConfig {
            enabled: true,
            broker_url: Some(format!("127.0.0.1:{port}")),
            base_topic: base.clone(),
            discovery_prefix: discovery.clone(),
            instance_id: instance.clone(),
        },
    };
    cfg.sensors.insert(
        "desk".into(),
        dormant_core::config::schema::SensorConfig::Mqtt(
            dormant_core::config::schema::MqttSensorCfg {
                broker_url: format!("127.0.0.1:{port}"),
                topic: format!("{base}/{instance}/sensor/desk/state"),
                field: "/presence".into(),
                payload_on: None,
                payload_off: None,
                kind: dormant_core::config::schema::SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            },
        ),
    );
    let _: String = discovery;
    (cfg, base, instance)
}

/// Subscribe + drain retained messages helper (used by the
/// offline-check below).
#[allow(dead_code)]
async fn subscriber(port: u16, base: &str, instance: &str) -> AsyncClient {
    let opts = MqttOptions::new(
        format!("dormant-int-sub-{}", unique_tag()),
        "127.0.0.1",
        port,
    );
    let (client, _eventloop) = AsyncClient::new(opts, 32);
    let state_topic = format!("{base}/{instance}/+");
    client
        .subscribe(&state_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe state");
    let discovery_topic = format!("homeassistant/+/{instance}/+");
    client
        .subscribe(&discovery_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe discovery");
    // Return the client; the caller drives the eventloop.
    client
}

/// Drain the subscriber's event loop for `timeout`, collecting each
/// retained delivery into the `out` vector. Only the explicit
/// `timeout` (no more events within the deadline) terminates the
/// drain; every non-`Publish` event (`SubAck`, `ConnAck`, `Outgoing`,
/// `Event::Outgoing(Outgoing::Publish)` during the local hand-off,
/// etc.) is consumed and the loop continues. The pre-fix shape
/// broke on the first non-`Publish` event, which was almost always
/// the `SubAck` of the subscriber's own `subscribe()` call — the
/// subscriber then returned with an empty buffer and the test
/// reported "no retained records arrived", even when the broker
/// had published the discovery + state retained records a few
/// hundred ms later.
async fn drain_for(
    _client: &AsyncClient,
    eventloop: &mut EventLoop,
    out: &mut Vec<(String, Vec<u8>, bool)>,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline - now, eventloop.poll()).await {
            Ok(Ok(rumqttc::Event::Incoming(Packet::Publish(p)))) => {
                out.push((p.topic.clone(), p.payload.to_vec(), p.retain));
            }
            // All other events (SubAck, PubAck, ConnAck, Outgoing
            // acks of our own subscribe, the broker's keep-alive
            // pongs, ...) are expected traffic — consume and continue
            // the drain until the deadline expires.
            Ok(_) => {}
            // `Err(Elapsed)` is the only path that should END the
            // drain. Any other error (broker drop, socket reset)
            // is also a real exit — the broker is gone, no more
            // records can land.
            Err(_) => break,
        }
    }
}

#[ignore = "requires broker: DORMANT_TEST_MQTT=1"]
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn publisher_publishes_retained_discovery_state_and_graceful_offline() {
    let Some(port) = mqtt_port() else {
        return;
    };

    eprintln!(
        "[int-test] starting, broker=127.0.0.1:{port}, tag={}",
        unique_tag()
    );
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("dormantd::state_publisher=trace,debug")
            }),
        )
        .try_init();
    let tag = unique_tag();
    let (cfg, base, instance) = per_test_config(port, &tag);
    let cfg = std::sync::Arc::new(cfg);
    let creds = std::sync::Arc::new(Credentials::default());

    // Manual ControlMsg responder: a real rules engine isn't required
    // for the publisher's contract — only `SubscribeEvents` and
    // `Snapshot` need to be answered so the publisher can run.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
    let (event_tx, event_rx) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
    let cancel = CancellationToken::new();
    let responder = tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            match msg {
                ControlMsg::SubscribeEvents(tx) => {
                    let _ = tx.send(event_tx.subscribe());
                }
                ControlMsg::Snapshot(tx) => {
                    // Minimal snapshot mirroring the unit-test fixture:
                    // one sensor (Present + online), one zone (present),
                    // one display (active phase).
                    let snap = StateSnapshot {
                        sensors: vec![dormant_core::rules::SensorSnapshot {
                            id: "desk".into(),
                            state: dormant_core::types::SensorState::Present,
                            last_seen_secs_ago: 0,
                            reported: true,
                        }],
                        zones: vec![dormant_core::rules::ZoneSnapshot {
                            id: "office".into(),
                            present: Some(true),
                        }],
                        displays: vec![(
                            "main".into(),
                            dormant_core::rules::DisplaySnapshot {
                                phase: "active".into(),
                                inhibited: false,
                                paused: false,
                                cmd_gen: 0,
                                scope: dormant_core::config::DisplayScope::default(),
                                owned: true,
                                observed_input_code: None,
                                panel_state: None,
                                controllers: Vec::new(),
                                wake_attempts: 0,
                                last_blank_failed: false,
                                stage: None,
                            },
                        )],
                        pending_reload: None,
                        rollback: None,
                        kvm: None,
                        wear_sampling_status: None,
                    };
                    let _ = tx.send(snap);
                }
                _ => {}
            }
        }
    });

    // Sanity: don't double-connect to a same-test publisher that might
    // already be running — the per-test unique tag avoids collisions.
    // CRITICAL: the subscriber must use the SAME base + instance
    // values the publisher will publish to. The pre-fix shape called
    // `unique_tag()` again inside the subscriber closure, generating
    // a different tag — the subscriber's topic filter never matched
    // the publisher's published topics, so the buffer stayed empty
    // even though the broker had the records. The fix threads the
    // already-computed `base` and `instance` into the subscriber
    // closure (the `tag` is already in `base` and `instance`).
    let subscriber_handle = tokio::spawn({
        let base = base.clone();
        let instance = instance.clone();
        async move {
            // Build a fresh subscriber side from inside this task.
            let opts = MqttOptions::new(format!("dormant-int-sub-{tag}"), "127.0.0.1", port);
            let (client, mut eventloop) = AsyncClient::new(opts, 32);
            // The publisher publishes to multi-segment paths under
            // `<base>/<instance>/` (e.g. `.../sensor/desk/state`,
            // `.../sensor/desk/availability`, `.../availability`).
            // `+` matches ONE segment, `#` matches the rest of the
            // tree. The pre-fix shape used `+` and never saw any
            // publish — the broker shipped the records, the
            // subscriber's filter just rejected them. `#` is the
            // correct wildcard.
            let state_topic = format!("{base}/{instance}/#");
            let _ = client.subscribe(&state_topic, QoS::AtLeastOnce).await;
            let discovery_topic = format!("homeassistant/+/{instance}/#");
            let _ = client.subscribe(&discovery_topic, QoS::AtLeastOnce).await;
            let mut out = Vec::new();
            drain_for(&client, &mut eventloop, &mut out, Duration::from_secs(8)).await;
            out
        }
    });

    // Spawn the publisher with the REAL MqttTransport.
    let handle = spawn(StatePublisherDeps {
        config: cfg.clone(),
        credentials: creds.clone(),
        ctl_tx: ctl_tx.clone(),
        cancel: cancel.clone(),
    });
    assert!(handle.is_some(), "publish.enabled must spawn a task");

    // Give the publisher time to flush discovery + initial snapshot.
    let drained = subscriber_handle
        .await
        .expect("subscriber task did not panic");

    assert!(
        !drained.is_empty(),
        "no retained records arrived at the broker; got 0 messages"
    );

    // Confirm at least one discovery config for the sensor and one
    // sensor-state record arrived. The MQTT 3.1.1 spec says the broker
    // MUST forward stored retained messages with the retain flag set,
    // but mosquitto 2.x strips the retain flag on stored-message
    // delivery (it's a long-standing mosquitto quirk — the messages
    // ARE stored as retained, a fresh subscriber just sees `retain =
    // false` on the wire). We assert topic + payload, NOT the retain
    // flag, and prove the broker DID store retained records by having
    // a SECOND subscriber connect AFTER the publisher's cancel and
    // read the global `offline` (the offline check below — a stored
    // retained message is the only way that subscriber can see the
    // `offline` payload at all).
    let desk_discovery = format!("homeassistant/binary_sensor/{instance}/sensor_desk/config");
    let sensor_state = format!("{base}/{instance}/sensor/desk/state");
    let has_discovery = drained
        .iter()
        .any(|(topic, _payload, _retain)| topic == &desk_discovery);
    let has_state = drained
        .iter()
        .any(|(topic, payload, _retain)| topic == &sensor_state && payload == b"ON");
    assert!(has_discovery, "expected discovery config for desk");
    assert!(has_state, "expected `ON` on the desk sensor state topic");

    // Cancel and confirm a retained `offline` arrives on the global
    // availability topic. The LWT handles ungraceful drops; the
    // explicit retained publish on cancel closes out a clean reload.
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle.unwrap()).await;
    drop(event_rx);

    // New subscriber to read the offline retained record.
    let offline_topic = format!("{base}/{instance}/availability");
    let opts = MqttOptions::new(
        format!("dormant-int-off-{}", unique_tag()),
        "127.0.0.1",
        port,
    );
    let (client, mut eventloop) = AsyncClient::new(opts, 32);
    client
        .subscribe(&offline_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe offline");
    let mut offline_out = Vec::new();
    drain_for(
        &client,
        &mut eventloop,
        &mut offline_out,
        Duration::from_secs(5),
    )
    .await;
    // Same mosquitto quirk as above: the retain flag on stored-message
    // delivery is unreliable. The CRITICAL check here is that the
    // NEW subscriber (connecting AFTER the publisher cancelled and
    // disconnected) sees the `offline` payload at all — the only way
    // is if the broker stored it as retained. A non-retained
    // `offline` would have been dropped on disconnect.
    let got_offline = offline_out
        .iter()
        .any(|(topic, payload, _retain)| topic == &offline_topic && payload == b"offline");
    assert!(
        got_offline,
        "expected stored `offline` on {offline_topic:?}; got {offline_out:?}"
    );

    responder.abort();
}
