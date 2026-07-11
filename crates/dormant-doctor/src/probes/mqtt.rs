//! MQTT sensor probe — connects to the broker and subscribes to topic.

use crate::types::ProbeResult;
use dormant_core::config::schema::{Credentials, MqttCredential, MqttSensorCfg};
use dormant_core::types::SensorState;
use dormant_sensors::mqtt::parse_payload;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::time::Duration;

// ── Public API ───────────────────────────────────────────────────────────────────

/// Probe all MQTT-configured sensors.
pub async fn probe_mqtt_all(
    cfg: &dormant_core::config::Config,
    creds: &Credentials,
) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        if let dormant_core::config::schema::SensorConfig::Mqtt(mqtt_cfg) = sensor_cfg {
            results.push(probe_mqtt_one(id, mqtt_cfg, creds).await);
        }
    }
    if results.is_empty() {
        results.push(ProbeResult::skip("mqtt", "no MQTT sensors configured"));
    }
    results
}

/// Probe a single MQTT sensor by subscribing to its topic.
pub(crate) async fn probe_mqtt_one(
    id: &str,
    cfg: &MqttSensorCfg,
    creds: &Credentials,
) -> ProbeResult {
    let name = format!("mqtt {id}");

    let (mqttopts, _username) = probe_options(id, cfg, creds);
    let (client, mut eventloop) = AsyncClient::new(mqttopts, 100);

    // Subscribe to the sensor topic.
    if let Err(e) = client.subscribe(&cfg.topic, QoS::AtLeastOnce).await {
        return ProbeResult::fail(name, format!("subscribe failed: {e}"));
    }

    // Wait up to 10s for a retained/live message.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found: Option<SensorState> = None;
    let mut broker_connected = false;
    // Track the credential lookup result for auth-failure detail.
    let credential_lookup = creds.mqtt.get(&cfg.broker_url);

    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        let result = tokio::time::timeout(timeout, eventloop.poll()).await;

        match result {
            Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)))) => {
                let state = parse_payload(cfg, &publish.payload);
                if let Some(s) = state {
                    found = Some(s);
                    break;
                }
            }
            Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_)))) => {
                broker_connected = true;
            }
            Ok(Ok(_)) => {} // Ignore other packets.
            Ok(Err(e)) => {
                // rumqttc does not re-export ConnectReturnCode, so we
                // check the Display output for the NotAuthorized reason
                // code to produce a more actionable diagnostic.
                let err_msg = e.to_string();
                if err_msg.contains("NotAuthorized") {
                    let _ = client.disconnect().await;
                    return ProbeResult::fail(
                        name,
                        not_authorized_detail(&cfg.broker_url, credential_lookup),
                    );
                }
                return ProbeResult::fail(name, format!("broker connection error: {e}"));
            }
            Err(_elapsed) => break, // timeout
        }
    }

    let _ = client.disconnect().await;

    match found {
        Some(state) => {
            let state_str = match state {
                SensorState::Present => "present",
                SensorState::Absent => "absent",
                SensorState::Unavailable => "unavailable",
            };
            ProbeResult::pass(name, format!("topic '{}' reports {state_str}", cfg.topic))
        }
        None => {
            if broker_connected {
                ProbeResult::skip(
                    name,
                    format!(
                        "no message in 10s on '{}' — on-change sensors are quiet when state is stable; not a failure",
                        cfg.topic,
                    ),
                )
            } else {
                ProbeResult::fail(
                    name,
                    "broker connection failed (no CONNACK received)".to_string(),
                )
            }
        }
    }
}

// ── Pure helpers — testable without a broker ─────────────────────────────────────

/// Build `MqttOptions` for a doctor probe, applying credentials when
/// `creds.mqtt` has an entry keyed by the exact `cfg.broker_url`.
///
/// Returns the options and the credential that was applied (if any), so
/// tests can assert credential application without inspecting opaque
/// `MqttOptions` internals.
fn probe_options(
    id: &str,
    cfg: &MqttSensorCfg,
    creds: &Credentials,
) -> (MqttOptions, Option<MqttCredential>) {
    let (host, port) = parse_broker_url(&cfg.broker_url);
    let client_id = format!("dormant-doctor-{id}-{}", std::process::id());
    let mut mqttopts = MqttOptions::new(&client_id, host, port);
    mqttopts.set_clean_session(true);

    let applied = creds.mqtt.get(&cfg.broker_url).cloned();
    if let Some(ref cred) = applied {
        mqttopts.set_credentials(cred.username.clone(), cred.password.clone());
    }

    (mqttopts, applied)
}

/// Build the detail string for a `NotAuthorized` connection failure.
///
/// When no credential entry matched the broker URL, the message tells the
/// operator exactly which key to add.  When credentials *were* supplied,
/// the message names the rejected user so the operator can check the password.
fn not_authorized_detail(broker_url: &str, credential: Option<&MqttCredential>) -> String {
    match credential {
        Some(cred) => format!("authentication rejected for user '{}'", cred.username),
        None => format!(
            "broker requires auth and no credentials entry matches '{broker_url}' \
             — add [mqtt.\"{broker_url}\"] to credentials.toml"
        ),
    }
}

/// Parse a broker URL into (host, port).
#[must_use]
pub(crate) fn parse_broker_url(url: &str) -> (&str, u16) {
    let rest = url
        .strip_prefix("tcp://")
        .or_else(|| url.strip_prefix("mqtt://"))
        .unwrap_or(url);
    if let Some((host, port_str)) = rest.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
    {
        return (host, port);
    }
    (rest, 1883)
}

// ── Tests ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn test_creds(mqtt_entries: IndexMap<String, MqttCredential>) -> Credentials {
        Credentials {
            ha_token: None,
            samsung: IndexMap::new(),
            mqtt: mqtt_entries,
        }
    }

    fn test_mqtt_cfg(broker_url: impl Into<String>) -> MqttSensorCfg {
        MqttSensorCfg {
            broker_url: broker_url.into(),
            topic: "sensors/test".into(),
            field: "/occupancy".into(),
            payload_on: None,
            payload_off: None,
            kind: dormant_core::config::schema::SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
            availability_topic: None,
            availability_payload_online: "online".into(),
            availability_payload_offline: "offline".into(),
        }
    }

    // ── parse_broker_url ──────────────────────────────────────────────────────

    #[test]
    fn parse_broker_url_tcp() {
        let (host, port) = parse_broker_url("tcp://mqtt.local:1883");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_plain() {
        let (host, port) = parse_broker_url("127.0.0.1:1883");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_default_port() {
        let (host, port) = parse_broker_url("mqtt.local");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    // ── probe_options ─────────────────────────────────────────────────────────

    #[test]
    fn probe_options_applies_credentials_when_present() {
        let broker = "tcp://192.0.2.5:1883";
        let cfg = test_mqtt_cfg(broker);
        let creds = test_creds(IndexMap::from([(
            broker.into(),
            MqttCredential {
                username: "icetea".into(),
                password: "secret".into(),
            },
        )]));

        let (opts, applied) = probe_options("desk", &cfg, &creds);

        assert!(applied.is_some(), "credentials should be applied");
        assert_eq!(applied.unwrap().username, "icetea");
        // Verify via rumqttc's credentials getter that options carry the creds.
        assert!(
            opts.credentials().is_some(),
            "MqttOptions should carry credentials"
        );
        let (user, pwd) = opts.credentials().unwrap();
        assert_eq!(user, "icetea");
        assert_eq!(pwd, "secret");
    }

    #[test]
    fn probe_options_does_not_apply_credentials_when_absent() {
        let broker = "tcp://192.0.2.5:1883";
        let cfg = test_mqtt_cfg(broker);
        let creds = test_creds(IndexMap::new());

        let (opts, applied) = probe_options("desk", &cfg, &creds);

        assert!(applied.is_none(), "no credentials should be applied");
        assert!(
            opts.credentials().is_none(),
            "MqttOptions should have no credentials"
        );
    }

    #[test]
    fn probe_options_ignores_non_matching_broker_url() {
        let cfg = test_mqtt_cfg("tcp://192.0.2.5:1883");
        let creds = test_creds(IndexMap::from([(
            "tcp://other-broker:1883".into(),
            MqttCredential {
                username: "someone".into(),
                password: "else".into(),
            },
        )]));

        let (opts, applied) = probe_options("desk", &cfg, &creds);

        assert!(applied.is_none());
        assert!(opts.credentials().is_none());
    }

    // ── not_authorized_detail ─────────────────────────────────────────────────

    #[test]
    fn not_authorized_detail_no_matching_entry() {
        let detail = not_authorized_detail("tcp://192.0.2.5:1883", None);
        assert!(
            detail.contains("broker requires auth and no credentials entry matches"),
            "detail should contain the grep-stable prefix; got: {detail}"
        );
        assert!(
            detail.contains("tcp://192.0.2.5:1883"),
            "detail should name the broker URL; got: {detail}"
        );
        assert!(
            detail.contains("add [mqtt.\"tcp://192.0.2.5:1883\"]"),
            "detail should include actionable fix; got: {detail}"
        );
    }

    #[test]
    fn not_authorized_detail_rejected_credentials() {
        let cred = MqttCredential {
            username: "icetea".into(),
            password: "wrong".into(),
        };
        let detail = not_authorized_detail("tcp://192.0.2.5:1883", Some(&cred));
        assert_eq!(detail, "authentication rejected for user 'icetea'");
    }
}
