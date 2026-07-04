//! Explicit sensor-source registry — no macro magic (AGENTS.md rule 4).
//!
//! Each sensor type registers itself here.

use dormant_core::config::schema::{Credentials, MqttSensorCfg, SensorConfig};
use dormant_core::error::DormantError;
use dormant_core::traits::SensorSource;
use dormant_core::types::SensorId;
use indexmap::IndexMap;

use crate::ha_ws::HaWsSource;
use crate::mqtt::MqttSource;
use crate::usb_ld2410::UsbLd2410Source;

/// All recognised sensor `type` strings.
///
/// Used by `dormantctl doctor` and config validation to enumerate known types.
pub const SOURCE_TYPES: &[&str] = &["mqtt", "ha", "usb-ld2410"];

/// Build all sensor sources from the configuration map and credentials.
///
/// Sensors are grouped by `broker_url` for MQTT sources (one connection per
/// broker) and by `url` for HA WebSocket sources (one connection per HA
/// instance).
///
/// # Errors
///
/// Returns [`DormantError`] on configuration problems:
/// - `E_CREDS_MISSING` if an HA sensor is configured but `ha_token` is `None`.
pub fn build(
    sensors: &IndexMap<String, SensorConfig>,
    creds: &Credentials,
) -> Result<Vec<Box<dyn SensorSource>>, DormantError> {
    // Group MQTT sensors by broker_url.
    let mut by_broker: IndexMap<String, Vec<(SensorId, MqttSensorCfg)>> = IndexMap::new();
    let mut usb_sources: Vec<Box<dyn SensorSource>> = Vec::new();

    // Group HA sensors by url.
    let mut by_ha_url: IndexMap<String, Vec<(SensorId, String)>> = IndexMap::new();

    for (name, config) in sensors {
        match config {
            SensorConfig::Mqtt(cfg) => {
                let id = SensorId(name.clone());
                by_broker
                    .entry(cfg.broker_url.clone())
                    .or_default()
                    .push((id, cfg.clone()));
            }
            SensorConfig::Ha(cfg) => {
                let id = SensorId(name.clone());
                by_ha_url
                    .entry(cfg.url.clone())
                    .or_default()
                    .push((id, cfg.entity.clone()));
            }
            SensorConfig::UsbLd2410(cfg) => {
                let id = SensorId(name.clone());
                usb_sources
                    .push(Box::new(UsbLd2410Source::new(id, cfg.clone())) as Box<dyn SensorSource>);
            }
        }
    }

    let mut sources: Vec<Box<dyn SensorSource>> = Vec::new();

    // MQTT sources.
    for (broker_url, sensors) in by_broker {
        sources.push(Box::new(MqttSource::new(broker_url, sensors)) as Box<dyn SensorSource>);
    }

    // HA WebSocket sources — only require token when HA sensors exist.
    if !by_ha_url.is_empty() {
        let token = creds
            .ha_token
            .as_ref()
            .ok_or_else(|| DormantError::CredsMissing {
                what: "ha_token".into(),
            })?;

        for (url, entities) in by_ha_url {
            sources.push(Box::new(HaWsSource::new(url, token.clone(), entities)));
        }
    }

    // USB serial sources (one per sensor).
    sources.extend(usb_sources);

    Ok(sources)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use dormant_core::config::schema::{MqttSensorCfg, SensorKind};

    /// Credentials with a valid HA token.
    fn creds_with_token() -> Credentials {
        Credentials {
            ha_token: Some("test_ha_token".into()),
            ..Credentials::default()
        }
    }

    /// Credentials without an HA token.
    fn creds_no_token() -> Credentials {
        Credentials::default()
    }

    fn mqtt_cfg(broker: &str, topic: &str) -> SensorConfig {
        SensorConfig::Mqtt(MqttSensorCfg {
            broker_url: broker.into(),
            topic: topic.into(),
            field: "/occupancy".into(),
            payload_on: None,
            payload_off: None,
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
        })
    }

    #[test]
    fn build_groups_by_broker() {
        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "desk".into(),
            mqtt_cfg("tcp://broker1:1883", "sensors/desk"),
        );
        sensors.insert(
            "couch".into(),
            mqtt_cfg("tcp://broker1:1883", "sensors/couch"),
        );
        sensors.insert(
            "door".into(),
            mqtt_cfg("tcp://broker2:1883", "sensors/door"),
        );

        let sources = build(&sensors, &creds_with_token()).unwrap();
        assert_eq!(sources.len(), 2, "two distinct brokers → two sources");

        // Source IDs are broker URLs.
        let ids: Vec<&str> = sources.iter().map(|s| s.source_id()).collect();
        assert!(ids.contains(&"tcp://broker1:1883"));
        assert!(ids.contains(&"tcp://broker2:1883"));
    }

    #[test]
    fn build_handles_mqtt_and_usb_configs() {
        use dormant_core::config::schema::{HaSensorCfg, UsbLd2410Cfg};

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "mqtt_sensor".into(),
            mqtt_cfg("tcp://broker:1883", "test/topic"),
        );
        sensors.insert(
            "ha_sensor".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.test".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );
        sensors.insert(
            "usb_sensor".into(),
            SensorConfig::UsbLd2410(UsbLd2410Cfg {
                port: "/dev/ttyUSB0".into(),
                baud: 256_000,
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );

        let sources = build(&sensors, &creds_with_token()).unwrap();
        // MQTT (1) + HA (1) + USB (1) = 3 sources.
        assert_eq!(sources.len(), 3, "MQTT + HA + USB sources are built");
        let ids: Vec<&str> = sources.iter().map(|s| s.source_id()).collect();
        assert!(ids.contains(&"tcp://broker:1883"));
        assert!(ids.contains(&"ws://ha.local:8123/api/websocket"));
        assert!(ids.contains(&"usb_sensor"));
    }

    #[test]
    fn build_empty_map_returns_empty_vec() {
        let sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        let sources = build(&sensors, &creds_with_token()).unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn source_types_contains_known_types() {
        assert!(SOURCE_TYPES.contains(&"mqtt"));
        assert!(SOURCE_TYPES.contains(&"usb-ld2410"));
    }

    #[test]
    fn source_types_contains_ha() {
        assert!(SOURCE_TYPES.contains(&"ha"));
    }

    #[test]
    fn build_constructs_ha_source_with_token() {
        use dormant_core::config::schema::HaSensorCfg;

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "motion".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.motion".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );

        let sources = build(&sensors, &creds_with_token()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_id(), "ws://ha.local:8123/api/websocket");
    }

    #[test]
    fn build_missing_ha_token_errors() {
        use dormant_core::config::schema::HaSensorCfg;

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "motion".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.motion".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );

        match build(&sensors, &creds_no_token()) {
            Err(err) => {
                assert!(
                    err.to_string().contains("E_CREDS_MISSING"),
                    "error should mention missing credentials: {}",
                    err,
                );
            }
            Ok(_) => panic!("expected Err for missing ha_token"),
        }
    }

    #[test]
    fn build_two_ha_sensors_same_url_one_source() {
        use dormant_core::config::schema::HaSensorCfg;

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "motion".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.motion".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );
        sensors.insert(
            "door".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.door".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );

        let sources = build(&sensors, &creds_with_token()).unwrap();
        assert_eq!(sources.len(), 1, "same URL → one source");
        assert_eq!(sources[0].source_id(), "ws://ha.local:8123/api/websocket");
    }

    #[test]
    fn build_mqtt_only_without_token_succeeds() {
        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert("desk".into(), mqtt_cfg("tcp://broker:1883", "sensors/desk"));

        // No HA sensors → no token required.
        let sources = build(&sensors, &creds_no_token()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_id(), "tcp://broker:1883");
    }

    #[test]
    fn build_groups_ha_by_url() {
        use dormant_core::config::schema::HaSensorCfg;

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "motion".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha1.local:8123/api/websocket".into(),
                entity: "binary_sensor.motion".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );
        sensors.insert(
            "door".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha2.local:8123/api/websocket".into(),
                entity: "binary_sensor.door".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );

        let sources = build(&sensors, &creds_with_token()).unwrap();
        assert_eq!(sources.len(), 2, "two distinct HA URLs → two sources");
        let ids: Vec<&str> = sources.iter().map(|s| s.source_id()).collect();
        assert!(ids.contains(&"ws://ha1.local:8123/api/websocket"));
        assert!(ids.contains(&"ws://ha2.local:8123/api/websocket"));
    }
}
