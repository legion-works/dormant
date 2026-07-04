//! Explicit sensor-source registry — no macro magic (AGENTS.md rule 4).
//!
//! Each sensor type registers itself here.  Tasks 9–10 will append their
//! source types and build arms to this module.

use dormant_core::config::schema::{MqttSensorCfg, SensorConfig};
use dormant_core::traits::SensorSource;
use dormant_core::types::SensorId;
use indexmap::IndexMap;

use crate::mqtt::MqttSource;

/// All recognised sensor `type` strings.
///
/// Used by `dormantctl doctor` and config validation to enumerate known types.
pub const SOURCE_TYPES: &[&str] = &["mqtt"];

/// Build all sensor sources from the configuration map.
///
/// Sensors are grouped by `broker_url` for MQTT sources (one connection per
/// broker).  Non-MQTT entries are silently ignored — Tasks 9–10 will extend
/// this function to handle `ha` and `usb-ld2410` types.
///
/// # Errors
///
/// Returns [`dormant_core::error::DormantError`] on configuration problems
/// (currently none for MQTT — reserved for future validation).
pub fn build(
    sensors: &IndexMap<String, SensorConfig>,
) -> Result<Vec<Box<dyn SensorSource>>, dormant_core::error::DormantError> {
    // Group MQTT sensors by broker_url.
    let mut by_broker: IndexMap<String, Vec<(SensorId, MqttSensorCfg)>> = IndexMap::new();

    for (name, config) in sensors {
        if let SensorConfig::Mqtt(cfg) = config {
            let id = SensorId(name.clone());
            by_broker
                .entry(cfg.broker_url.clone())
                .or_default()
                .push((id, cfg.clone()));
        }
    }

    let sources: Vec<Box<dyn SensorSource>> = by_broker
        .into_iter()
        .map(|(broker_url, sensors)| {
            Box::new(MqttSource::new(broker_url, sensors)) as Box<dyn SensorSource>
        })
        .collect();

    Ok(sources)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use dormant_core::config::schema::{MqttSensorCfg, SensorKind};

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

        let sources = build(&sensors).unwrap();
        assert_eq!(sources.len(), 2, "two distinct brokers → two sources");

        // Source IDs are broker URLs.
        let ids: Vec<&str> = sources.iter().map(|s| s.source_id()).collect();
        assert!(ids.contains(&"tcp://broker1:1883"));
        assert!(ids.contains(&"tcp://broker2:1883"));
    }

    #[test]
    fn build_ignores_non_mqtt_configs() {
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

        let sources = build(&sensors).unwrap();
        assert_eq!(sources.len(), 1, "only MQTT sources are built");
        assert_eq!(sources[0].source_id(), "tcp://broker:1883");
    }

    #[test]
    fn build_empty_map_returns_empty_vec() {
        let sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        let sources = build(&sensors).unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn source_types_contains_mqtt() {
        assert!(SOURCE_TYPES.contains(&"mqtt"));
    }
}
