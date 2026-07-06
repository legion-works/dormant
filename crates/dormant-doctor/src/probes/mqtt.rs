//! MQTT sensor probe — connects to the broker and subscribes to topic.

use crate::types::ProbeResult;
use dormant_core::config::schema::MqttSensorCfg;
use dormant_core::types::SensorState;
use dormant_sensors::mqtt::parse_payload;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::time::Duration;

/// Probe all MQTT-configured sensors.
pub async fn probe_mqtt_all(cfg: &dormant_core::config::Config) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        if let dormant_core::config::schema::SensorConfig::Mqtt(mqtt_cfg) = sensor_cfg {
            results.push(probe_mqtt_one(id, mqtt_cfg).await);
        }
    }
    if results.is_empty() {
        results.push(ProbeResult::skip("mqtt", "no MQTT sensors configured"));
    }
    results
}

/// Probe a single MQTT sensor by subscribing to its topic.
pub(crate) async fn probe_mqtt_one(id: &str, cfg: &MqttSensorCfg) -> ProbeResult {
    let name = format!("mqtt {id}");

    // Parse broker URL.
    let (host, port) = parse_broker_url(&cfg.broker_url);

    // Build a temporary MQTT client.
    let client_id = format!("dormant-doctor-{id}-{}", std::process::id());
    let mut mqttopts = MqttOptions::new(&client_id, host, port);
    mqttopts.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(mqttopts, 100);

    // Subscribe to the sensor topic.
    if let Err(e) = client.subscribe(&cfg.topic, QoS::AtLeastOnce).await {
        return ProbeResult::fail(name, format!("subscribe failed: {e}"));
    }

    // Wait up to 10s for a retained/live message.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found: Option<SensorState> = None;
    let mut broker_connected = false;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
