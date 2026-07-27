//! Pure state → MQTT publish-record mapping for issue #105.
//!
//! Implements the topic, payload, and collision contract ratified in
//! `.opencode/decisions/2026-07-27-mqtt-publish-contract.md`. NO I/O:
//! the three public entry points return `Vec<PublishRecord>` for the
//! publisher task to forward to `rumqttc`. Keeping these functions
//! pure is the same split the wear tracker uses (see `wear_tracker.rs`):
//! testing each one in isolation without bringing up a broker is the
//! only way the topic / payload literals stay grep-stable across
//! refactors.
//!
//! ## Module split
//!
//! - [`sanitize_topic_id`] — public sanitizer; the contract's only
//!   pure-data helper. Used by every id that lands in a topic.
//! - [`EntityInventory`] — collects the per-kind id lists from a
//!   loaded [`Config`] in config-order. `IndexMap` ordering is the
//!   "first wins" half of the collision rule.
//! - [`discovery_records`] — discovery configs for sensors, zones,
//!   and displays. One retained record per entity, every entity
//!   references the global LWT availability topic, and sensor
//!   entities ADDITIONALLY reference their per-sensor availability
//!   topic with `availability_mode: "all"`.
//! - [`snapshot_records`] — startup / re-connect flush: every
//!   retained state + per-sensor availability record derived from
//!   a [`StateSnapshot`].
//! - [`event_records`] — incremental single-event mapping.
//!   Returns an empty vec for unknown / foreign event variants —
//!   the contract is explicit that the publisher NEVER speculates
//!   about events the engine did not actually emit.
//!
//! ## Disabled-by-default
//!
//! Every entry point returns an empty `Vec` when
//! `cfg.publish.enabled == false`. The publisher task is the only
//! caller and it gates the call site on the same flag; the check
//! here is defensive (and keeps the pure functions easy to
//! unit-test without a real broker).

use std::collections::HashSet;

use serde_json::json;

use dormant_core::config::schema::Config;
use dormant_core::rules::{DaemonEvent, DisplaySnapshot, StateSnapshot};
use dormant_core::types::SensorState;

// ── Public record type ──────────────────────────────────────────────────────

/// One record for the publisher task to forward to `rumqttc`.
///
/// `topic` and `payload` are the contract's literal strings; `qos`
/// is always 1 and `retain` is always `true` for every record this
/// module emits (the public fields exist for the publisher task's
/// pin-rationale and to keep the wire shape greppable).
#[derive(Debug, Clone, PartialEq)]
pub struct PublishRecord {
    /// MQTT topic (already sanitized).
    pub topic: String,
    /// JSON payload (already serialized).
    pub payload: String,
    /// MQTT `QoS` — always `1` for this contract.
    pub qos: u8,
    /// MQTT retain — always `true` for this contract.
    pub retain: bool,
}

const PUBLISH_QOS: u8 = 1;
const RETAIN: bool = true;

// ── Public sanitizer (contract's only data helper) ──────────────────────────

/// Sanitize a raw id for use in a topic. Lowercase, ASCII-safe
/// `[a-z0-9_-]`, collapse runs of `_`, trim leading/trailing `_`,
/// empty result → `"unnamed"`. Reused by every id that lands in a
/// topic — `{instance}`, `{sensor_id}`, `{zone_id}`, `{display_id}`
/// — so a single helper change is the only seam for sanitizer
/// semantics.
///
/// `unnamed` (not `"dormant"`) is the contract's deliberate
/// collision-warning signal: two `""` ids do NOT silently map to
/// `"dormant"` and get a free pass — they both sanitize to
/// `"unnamed"` and the collision rule fires. The contract treats
/// this as a real collision (a WARN with both raw ids).
#[must_use]
pub fn sanitize_topic_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    // Collapse runs of `_` and trim leading/trailing `_`.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_was_underscore = true; // suppress leading underscore
    for c in out.chars() {
        if c == '_' {
            if !last_was_underscore {
                collapsed.push('_');
            }
            last_was_underscore = true;
        } else {
            collapsed.push(c);
            last_was_underscore = false;
        }
    }
    while collapsed.ends_with('_') {
        collapsed.pop();
    }
    if collapsed.is_empty() {
        "unnamed".to_string()
    } else {
        collapsed
    }
}

// ── Inventory ────────────────────────────────────────────────────────────────

/// All entity ids the publisher will discover, in config-order.
///
/// `IndexMap` iteration order is config-order, which is the
/// "first wins" half of the collision rule (see
/// [`detect_and_warn_collisions`]). The three fields are flat
/// strings — the actual topic-segment ids are produced at
/// call-site via [`sanitize_topic_id`].
#[derive(Debug, Clone)]
pub struct EntityInventory<'a> {
    /// Sensor ids in config order.
    pub sensors: Vec<&'a str>,
    /// Zone ids in config order.
    pub zones: Vec<&'a str>,
    /// Display ids in config order.
    pub displays: Vec<&'a str>,
}

impl<'a> EntityInventory<'a> {
    /// Build the inventory from the operator's loaded config.
    #[must_use]
    pub fn from_config(cfg: &'a Config) -> Self {
        Self {
            sensors: cfg.sensors.keys().map(String::as_str).collect(),
            zones: cfg.zones.keys().map(String::as_str).collect(),
            displays: cfg.displays.keys().map(String::as_str).collect(),
        }
    }
}

// ── Topic builders (the contract's only literal topic shapes) ────────────────

fn topic_availability(base: &str, instance: &str) -> String {
    format!("{base}/{instance}/availability")
}

fn topic_sensor_state(base: &str, instance: &str, sensor_id: &str) -> String {
    format!("{base}/{instance}/sensor/{sensor_id}/state")
}

fn topic_sensor_availability(base: &str, instance: &str, sensor_id: &str) -> String {
    format!("{base}/{instance}/sensor/{sensor_id}/availability")
}

fn topic_zone_state(base: &str, instance: &str, zone_id: &str) -> String {
    format!("{base}/{instance}/zone/{zone_id}/state")
}

fn topic_display_phase(base: &str, instance: &str, display_id: &str) -> String {
    format!("{base}/{instance}/display/{display_id}/phase")
}

fn topic_discovery_sensor(discovery_prefix: &str, instance: &str, sensor_id: &str) -> String {
    format!("{discovery_prefix}/binary_sensor/{instance}/sensor_{sensor_id}/config")
}

fn topic_discovery_zone(discovery_prefix: &str, instance: &str, zone_id: &str) -> String {
    format!("{discovery_prefix}/binary_sensor/{instance}/zone_{zone_id}/config")
}

fn topic_discovery_display(discovery_prefix: &str, instance: &str, display_id: &str) -> String {
    format!("{discovery_prefix}/sensor/{instance}/display_{display_id}/config")
}

// ── Unique id builders (grep-stable: `dormant_{instance}_{kind}_{id}`) ────────

fn unique_id_sensor(instance: &str, sensor_id: &str) -> String {
    format!("dormant_{instance}_sensor_{sensor_id}")
}

fn unique_id_zone(instance: &str, zone_id: &str) -> String {
    format!("dormant_{instance}_zone_{zone_id}")
}

fn unique_id_display(instance: &str, display_id: &str) -> String {
    format!("dormant_{instance}_display_{display_id}")
}

// ── Discovery payloads ───────────────────────────────────────────────────────

/// Build the discovery payload for a sensor (`binary_sensor`).
fn discovery_payload_sensor(cfg: &Config, instance: &str, sensor_id: &str) -> serde_json::Value {
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let sensor_id = sanitize_topic_id(sensor_id);
    let avail_topic = topic_availability(&base, &instance);
    let sensor_avail_topic = topic_sensor_availability(&base, &instance, &sensor_id);
    json!({
        "name": format!("dormant {sensor_id} presence"),
        "unique_id": unique_id_sensor(&instance, &sensor_id),
        "object_id": unique_id_sensor(&instance, &sensor_id),
        "state_topic": topic_sensor_state(&base, &instance, &sensor_id),
        "payload_on": "ON",
        "payload_off": "OFF",
        "availability": [
            {
                "topic": avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
            {
                "topic": sensor_avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
        ],
        "availability_mode": "all",
        "device": device_block(&instance),
    })
}

/// Build the discovery payload for a zone (`binary_sensor`).
fn discovery_payload_zone(cfg: &Config, instance: &str, zone_id: &str) -> serde_json::Value {
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let zone_id = sanitize_topic_id(zone_id);
    let avail_topic = topic_availability(&base, &instance);
    json!({
        "name": format!("dormant {zone_id} zone"),
        "unique_id": unique_id_zone(&instance, &zone_id),
        "object_id": unique_id_zone(&instance, &zone_id),
        "state_topic": topic_zone_state(&base, &instance, &zone_id),
        "payload_on": "ON",
        "payload_off": "OFF",
        "availability": [
            {
                "topic": avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
        ],
        "device": device_block(&instance),
    })
}

/// Build the discovery payload for a display (sensor, not `binary_sensor`).
///
/// The state value is the lowercase phase literal from
/// `DisplaySnapshot.phase` — read back through the `value_template`
/// so HA parses the field out of the published JSON object.
fn discovery_payload_display(cfg: &Config, instance: &str, display_id: &str) -> serde_json::Value {
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let display_id = sanitize_topic_id(display_id);
    let avail_topic = topic_availability(&base, &instance);
    json!({
        "name": format!("dormant {display_id} display"),
        "unique_id": unique_id_display(&instance, &display_id),
        "object_id": unique_id_display(&instance, &display_id),
        "state_topic": topic_display_phase(&base, &instance, &display_id),
        "value_template": "{{ value_json.phase }}",
        "availability": [
            {
                "topic": avail_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            },
        ],
        "device": device_block(&instance),
    })
}

fn device_block(instance: &str) -> serde_json::Value {
    json!({
        "identifiers": [format!("dormant_{instance}")],
        "name": format!("dormant {instance}"),
        "manufacturer": "dormant",
        "model": "presence",
        "sw_version": env!("CARGO_PKG_VERSION"),
    })
}

// ── Public entry points ─────────────────────────────────────────────────────

/// Discovery configs for every entity the inventory knows about.
///
/// Returned records are ordered sensors → zones → displays, and
/// within each kind in config-order. First-wins collisions are
/// resolved by [`detect_and_warn_collisions`]: a collision drops
/// every record after the first for that sanitized id and emits
/// one `publish_id_collision` WARN per colliding pair.
#[must_use]
pub fn discovery_records(
    cfg: &Config,
    inventory: &EntityInventory<'_>,
    instance: &str,
) -> Vec<PublishRecord> {
    if !cfg.publish.enabled {
        return Vec::new();
    }
    let _ = detect_and_warn_collisions(cfg, inventory);

    let instance = sanitize_topic_id(instance);
    let discovery_prefix = sanitize_topic_id(&cfg.publish.discovery_prefix);

    let mut records: Vec<PublishRecord> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Sensors (binary_sensor).
    for sensor_id in &inventory.sensors {
        let sanitized = sanitize_topic_id(sensor_id);
        if !seen.insert(sanitized.clone()) {
            continue;
        }
        let topic = topic_discovery_sensor(&discovery_prefix, &instance, &sanitized);
        let payload = discovery_payload_sensor(cfg, &instance, sensor_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    // Zones (binary_sensor).
    for zone_id in &inventory.zones {
        let sanitized = sanitize_topic_id(zone_id);
        if !seen.insert(sanitized.clone()) {
            continue;
        }
        let topic = topic_discovery_zone(&discovery_prefix, &instance, &sanitized);
        let payload = discovery_payload_zone(cfg, &instance, zone_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    // Displays (sensor).
    for display_id in &inventory.displays {
        let sanitized = sanitize_topic_id(display_id);
        if !seen.insert(sanitized.clone()) {
            continue;
        }
        let topic = topic_discovery_display(&discovery_prefix, &instance, &sanitized);
        let payload = discovery_payload_display(cfg, &instance, display_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    records
}

/// Startup / re-connect flush: every retained state record from a
/// full [`StateSnapshot`].
///
/// Each sensor contributes two records (state + availability); each
/// zone and each display contributes one. Disabled-by-default returns
/// an empty `Vec` (the contract's kill-switch).
#[must_use]
pub fn snapshot_records(
    cfg: &Config,
    snapshot: &StateSnapshot,
    instance: &str,
) -> Vec<PublishRecord> {
    if !cfg.publish.enabled {
        return Vec::new();
    }
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    let mut records: Vec<PublishRecord> = Vec::new();

    for sensor in &snapshot.sensors {
        let sensor_id = sanitize_topic_id(&sensor.id);
        records.push(state_record(
            &topic_sensor_state(&base, &instance, &sensor_id),
            sensor.state,
        ));
        records.push(availability_record(
            &topic_sensor_availability(&base, &instance, &sensor_id),
            sensor.state,
        ));
    }

    for zone in &snapshot.zones {
        let zone_id = sanitize_topic_id(&zone.id);
        // A zone that has never been resolved (`present = None`)
        // publishes "OFF" — absent-by-default, matching the
        // fail-toward-blanking policy in the engine.
        let payload = if zone.present.unwrap_or(false) {
            "ON"
        } else {
            "OFF"
        };
        let topic = topic_zone_state(&base, &instance, &zone_id);
        records.push(PublishRecord {
            topic,
            payload: payload.to_string(),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    for (display_id_raw, display) in &snapshot.displays {
        let display_id = sanitize_topic_id(display_id_raw);
        let topic = topic_display_phase(&base, &instance, &display_id);
        records.push(PublishRecord {
            topic,
            payload: display_phase_payload(display),
            qos: PUBLISH_QOS,
            retain: RETAIN,
        });
    }

    records
}

/// Incremental single-event mapping. Returns an empty `Vec` for
/// unknown / foreign event variants — the contract is explicit
/// that the publisher NEVER speculates about events the engine did
/// not actually emit.
///
/// The currently-mapped variants:
/// - [`DaemonEvent::SensorChanged`] → sensor state + sensor
///   availability
/// - [`DaemonEvent::ZoneChanged`] → zone state
/// - [`DaemonEvent::DisplayPhase`] → display phase
/// - [`DaemonEvent::Subscribed`] / [`DaemonEvent::ConfigReloaded`]
///   / wear / blank / wake / ownership / [`DaemonEvent::Unknown`] →
///   empty (not the publisher's plane)
#[must_use]
pub fn event_records(cfg: &Config, event: &DaemonEvent, instance: &str) -> Vec<PublishRecord> {
    if !cfg.publish.enabled {
        return Vec::new();
    }
    let base = sanitize_topic_id(&cfg.publish.base_topic);
    let instance = sanitize_topic_id(instance);
    match event {
        DaemonEvent::SensorChanged { sensor, state } => {
            let sensor_id = sanitize_topic_id(&sensor.0);
            vec![
                state_record(&topic_sensor_state(&base, &instance, &sensor_id), *state),
                availability_record(
                    &topic_sensor_availability(&base, &instance, &sensor_id),
                    *state,
                ),
            ]
        }
        DaemonEvent::ZoneChanged { zone, present, .. } => {
            let zone_id = sanitize_topic_id(&zone.0);
            let payload = if *present { "ON" } else { "OFF" };
            vec![PublishRecord {
                topic: topic_zone_state(&base, &instance, &zone_id),
                payload: payload.to_string(),
                qos: PUBLISH_QOS,
                retain: RETAIN,
            }]
        }
        DaemonEvent::DisplayPhase { display, phase, .. } => {
            let display_id = sanitize_topic_id(&display.0);
            vec![PublishRecord {
                topic: topic_display_phase(&base, &instance, &display_id),
                // The phase literal is the engine's own grep-stable
                // string (`active|grace|blanking|blanked|waking|staged`);
                // publishing it bare keeps HA's value_template honest.
                payload: phase.clone(),
                qos: PUBLISH_QOS,
                retain: RETAIN,
            }]
        }
        // Every other variant is intentionally a no-op: the
        // publisher only carries state, never operational events
        // (blank failure, wake retry, wear, ownership) which
        // already have their own diagnostic surfaces.
        DaemonEvent::Subscribed
        | DaemonEvent::ConfigReloaded
        | DaemonEvent::WakeRetry { .. }
        | DaemonEvent::WearSnapshot { .. }
        | DaemonEvent::CompensationAdvisory { .. }
        | DaemonEvent::BlankFailure { .. }
        | DaemonEvent::BlankRecovered { .. }
        | DaemonEvent::WakeRecovered { .. }
        | DaemonEvent::Ownership { .. }
        | DaemonEvent::Unknown => Vec::new(),
    }
}

// ── Private payload shapers ──────────────────────────────────────────────────

fn state_record(topic: &str, state: SensorState) -> PublishRecord {
    // HA binary_sensor defaults: `ON` for present, `OFF` for absent
    // or unavailable. Unavailable deliberately publishes OFF (the
    // same payload as absent) so a stale sensor and an empty
    // room read the same way to the consumer; the fail-safe
    // presence policy is enforced upstream by the engine, not
    // re-encoded here.
    let payload = match state {
        SensorState::Present => "ON",
        SensorState::Absent | SensorState::Unavailable => "OFF",
    };
    PublishRecord {
        topic: topic.to_string(),
        payload: payload.to_string(),
        qos: PUBLISH_QOS,
        retain: RETAIN,
    }
}

fn availability_record(topic: &str, state: SensorState) -> PublishRecord {
    let payload = match state {
        SensorState::Present | SensorState::Absent => "online",
        SensorState::Unavailable => "offline",
    };
    PublishRecord {
        topic: topic.to_string(),
        payload: payload.to_string(),
        qos: PUBLISH_QOS,
        retain: RETAIN,
    }
}

fn display_phase_payload(display: &DisplaySnapshot) -> String {
    // The display phase publishes a JSON object with the
    // `phase` field so HA's `value_template: "{{ value_json.phase }}"`
    // parses the literal. Carrying the empty JSON object is
    // intentional — a bare string would not parse through the
    // template. We do not re-emit the `inhibited` / `paused` flags
    // here; those have their own surfaces in the rules engine and
    // are out of scope for the publish contract.
    json!({ "phase": display.phase }).to_string()
}

// ── Collision detection (first-wins, WARN once) ─────────────────────────────

/// Returned to the caller for log-side effect; the public entry
/// points call this for its side-effect (a single `publish_id_collision`
/// WARN per colliding pair) and ignore the return value.
#[must_use]
pub fn detect_and_warn_collisions(
    cfg: &Config,
    inventory: &EntityInventory<'_>,
) -> Vec<(String, Vec<String>)> {
    let _ = cfg; // Reserved for a future per-broker scoping knob.
    let mut by_sanitized: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();

    let mut record = |raw: &str, kind: &str| {
        let sanitized = sanitize_topic_id(raw);
        // The `unnamed` collision is a contract signal: two empty
        // ids are the same kind of mistake as a real collision.
        let entry = by_sanitized.entry(sanitized.clone()).or_default();
        let key = format!("{kind}:{raw}");
        if !entry.contains(&key) {
            entry.push(key);
        }
        if !order.contains(&sanitized) {
            order.push(sanitized);
        }
    };

    for id in &inventory.sensors {
        record(id, "sensor");
    }
    for id in &inventory.zones {
        record(id, "zone");
    }
    for id in &inventory.displays {
        record(id, "display");
    }

    let mut collisions: Vec<(String, Vec<String>)> = Vec::new();
    for sanitized in &order {
        if let Some(entries) = by_sanitized.get(sanitized)
            && entries.len() > 1
        {
            tracing::warn!(
                event = "publish_id_collision",
                sanitized_id = %sanitized,
                raw_ids = ?entries,
                "two config ids sanitize to the same topic id; publishing only the first (config-order)"
            );
            collisions.push((sanitized.clone(), entries.clone()));
        }
    }
    collisions
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::state_publisher::{
        EntityInventory, detect_and_warn_collisions, discovery_records, event_records,
        sanitize_topic_id, snapshot_records,
    };
    use dormant_core::config::schema::{
        AudioConfig, Config, DisplayConfig, DisplayScope, MqttSensorCfg, NotificationsConfig,
        SensorConfig, SensorKind, WatchdogConfig, WearConfig, ZoneConfig,
    };
    use dormant_core::rules::{
        DaemonEvent, DisplaySnapshot, SensorSnapshot, StateSnapshot, ZoneSnapshot,
    };
    use dormant_core::types::{DisplayId, SensorId, ZoneId};
    use indexmap::IndexMap;
    use std::time::Duration;

    // ── Test fixtures ─────────────────────────────────────────────────────

    fn enabled_cfg() -> Config {
        let mut cfg = Config {
            config_version: 1,
            daemon: dormant_core::config::DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::new(),
            rules: IndexMap::new(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            coordination: dormant_core::config::CoordinationConfig::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig {
                enabled: true,
                broker_url: Some("tcp://h:1883".into()),
                base_topic: "dormant".into(),
                discovery_prefix: "homeassistant".into(),
                instance_id: "office-pc".into(),
            },
        };
        cfg.sensors.insert(
            "desk".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        cfg.zones.insert(
            "office".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        cfg.displays.insert(
            "main".into(),
            DisplayConfig {
                controllers: vec!["kwin-dpms".into()],
                scope: DisplayScope::default(),
                shared_input_code: None,
                shared_input_write_code: None,
                shared_peer_input_code: None,
                shared_peer_input_write_code: None,
                hooks: dormant_core::config::schema::HookSlots::default(),
                blank_mode: Some(dormant_core::types::BlankMode::PowerOff),
                degraded_mode: None,
                ladder: Vec::new(),
                screensaver: None,
                output: None,
                ddc_display: None,
                host: None,
                wol_mac: None,
                blank_command: None,
                wake_command: None,
                modes: None,
                ha_url: None,
                blank_service: None,
                blank_data: None,
                wake_service: None,
                wake_data: None,
                command_timeout: Duration::from_secs(10),
                restore_brightness: 80,
                samsung_restore_backlight: 50,
                treat_unreachable_as_blanked: true,
                panel_type: dormant_core::wear::PanelType::Unknown,
            },
        );
        cfg
    }

    fn disabled_cfg() -> Config {
        let mut cfg = enabled_cfg();
        cfg.publish.enabled = false;
        cfg
    }

    fn snapshot_one_of_each() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![SensorSnapshot {
                id: "desk".into(),
                state: SensorState::Present,
                last_seen_secs_ago: 0,
                reported: true,
            }],
            zones: vec![ZoneSnapshot {
                id: "office".into(),
                present: Some(true),
            }],
            displays: vec![(
                "main".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 0,
                    scope: DisplayScope::default(),
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
        }
    }

    // ── Sanitizer (the contract's only data helper) ─────────────────────

    #[test]
    fn sanitize_lowercases() {
        assert_eq!(sanitize_topic_id("HELLO"), "hello");
    }

    #[test]
    fn sanitize_keeps_dash_and_digit() {
        assert_eq!(sanitize_topic_id("zone-2_a"), "zone-2_a");
    }

    #[test]
    fn sanitize_maps_disallowed_chars_to_underscore() {
        assert_eq!(sanitize_topic_id("a b.c"), "a_b_c");
        assert_eq!(sanitize_topic_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_topic_id("a:b@c"), "a_b_c");
    }

    #[test]
    fn sanitize_collapses_runs_of_underscore() {
        assert_eq!(sanitize_topic_id("a  b"), "a_b");
        assert_eq!(sanitize_topic_id("a...b"), "a_b");
    }

    #[test]
    fn sanitize_trims_leading_and_trailing_underscore() {
        assert_eq!(sanitize_topic_id("__a__"), "a");
        assert_eq!(sanitize_topic_id("..."), "unnamed");
    }

    #[test]
    fn sanitize_empty_is_unnamed_not_dormant() {
        // Per the contract, empty ids are a real collision (both
        // sanitize to "unnamed"), NOT a free pass.
        assert_eq!(sanitize_topic_id(""), "unnamed");
    }

    // ── Disabled-by-default ─────────────────────────────────────────────

    #[test]
    fn discovery_records_empty_when_disabled() {
        let cfg = disabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        assert!(discovery_records(&cfg, &inv, "office-pc").is_empty());
    }

    #[test]
    fn snapshot_records_empty_when_disabled() {
        let cfg = disabled_cfg();
        let snap = snapshot_one_of_each();
        assert!(snapshot_records(&cfg, &snap, "office-pc").is_empty());
    }

    #[test]
    fn event_records_empty_when_disabled() {
        let cfg = disabled_cfg();
        let ev = DaemonEvent::SensorChanged {
            sensor: SensorId("desk".into()),
            state: SensorState::Present,
        };
        assert!(event_records(&cfg, &ev, "office-pc").is_empty());
    }

    // ── Discovery records: EXACT topic + payload literals ──────────────

    #[test]
    fn discovery_records_exact_topics_for_one_of_each() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let records = discovery_records(&cfg, &inv, "office-pc");
        let topics: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();

        // 1 sensor + 1 zone + 1 display = 3 records.
        assert_eq!(records.len(), 3, "topics: {topics:?}");
        assert!(topics.contains(&"homeassistant/binary_sensor/office-pc/sensor_desk/config"));
        assert!(topics.contains(&"homeassistant/binary_sensor/office-pc/zone_office/config"));
        assert!(topics.contains(&"homeassistant/sensor/office-pc/display_main/config"));
    }

    #[test]
    fn discovery_records_all_have_qos1_and_retained() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        for r in discovery_records(&cfg, &inv, "office-pc") {
            assert_eq!(r.qos, 1, "all records are QoS 1: {}", r.topic);
            assert!(r.retain, "all records are retained: {}", r.topic);
        }
    }

    #[test]
    fn discovery_records_stable_unique_ids() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        for r in &discovery_records(&cfg, &inv, "office-pc") {
            let v: serde_json::Value = serde_json::from_str(&r.payload).unwrap();
            let unique_id = v
                .get("unique_id")
                .and_then(serde_json::Value::as_str)
                .expect("unique_id present");
            let object_id = v
                .get("object_id")
                .and_then(serde_json::Value::as_str)
                .expect("object_id present");
            assert_eq!(unique_id, object_id, "object_id mirrors unique_id");
            assert!(
                unique_id.starts_with("dormant_office-pc_"),
                "unique_id prefix pinned: {unique_id}"
            );
            assert!(
                unique_id.contains("_sensor_")
                    || unique_id.contains("_zone_")
                    || unique_id.contains("_display_")
            );
        }
    }

    #[test]
    fn discovery_records_sensor_has_two_availability_entries_with_all_mode() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let sensor_record = discovery_records(&cfg, &inv, "office-pc")
            .into_iter()
            .find(|r| r.topic == "homeassistant/binary_sensor/office-pc/sensor_desk/config")
            .expect("sensor discovery");
        let v: serde_json::Value = serde_json::from_str(&sensor_record.payload).unwrap();
        let availability = v
            .get("availability")
            .and_then(serde_json::Value::as_array)
            .expect("availability array");
        assert_eq!(availability.len(), 2, "sensor has 2 availability entries");
        let topics: Vec<&str> = availability
            .iter()
            .filter_map(|entry| entry.get("topic").and_then(serde_json::Value::as_str))
            .collect();
        assert!(topics.contains(&"dormant/office-pc/availability"));
        assert!(topics.contains(&"dormant/office-pc/sensor/desk/availability"));
        assert_eq!(
            v.get("availability_mode")
                .and_then(serde_json::Value::as_str),
            Some("all")
        );
    }

    #[test]
    fn discovery_records_zone_has_only_global_availability() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let zone_record = discovery_records(&cfg, &inv, "office-pc")
            .into_iter()
            .find(|r| r.topic == "homeassistant/binary_sensor/office-pc/zone_office/config")
            .expect("zone discovery");
        let v: serde_json::Value = serde_json::from_str(&zone_record.payload).unwrap();
        let availability = v
            .get("availability")
            .and_then(serde_json::Value::as_array)
            .expect("availability array");
        assert_eq!(availability.len(), 1, "zone has 1 availability entry");
        assert_eq!(
            availability[0]
                .get("topic")
                .and_then(serde_json::Value::as_str),
            Some("dormant/office-pc/availability")
        );
    }

    #[test]
    fn discovery_records_display_uses_value_template() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let display_record = discovery_records(&cfg, &inv, "office-pc")
            .into_iter()
            .find(|r| r.topic == "homeassistant/sensor/office-pc/display_main/config")
            .expect("display discovery");
        let v: serde_json::Value = serde_json::from_str(&display_record.payload).unwrap();
        assert_eq!(
            v.get("value_template").and_then(serde_json::Value::as_str),
            Some("{{ value_json.phase }}")
        );
    }

    #[test]
    fn discovery_records_no_secret_bearing_fields() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        for r in discovery_records(&cfg, &inv, "office-pc") {
            for forbidden in ["password", "token", "secret", "username", "credential"] {
                assert!(
                    !r.payload.to_ascii_lowercase().contains(forbidden),
                    "discovery payload leaked credential-shaped field {forbidden:?}: {}",
                    r.payload
                );
            }
        }
    }

    #[test]
    fn discovery_records_instance_id_is_sanitized() {
        let cfg = enabled_cfg();
        let inv = EntityInventory::from_config(&cfg);
        let records = discovery_records(&cfg, &inv, "Weird Host!");
        let topics: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();
        // "Weird Host!" -> "weird_host"
        assert!(topics.contains(&"homeassistant/binary_sensor/weird_host/sensor_desk/config"));
    }

    // ── Snapshot records ───────────────────────────────────────────────

    #[test]
    fn snapshot_records_one_per_sensor_state_sensor_availability_zone_display() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        // 1 sensor (state + availability) + 1 zone + 1 display = 4.
        assert_eq!(records.len(), 4, "records: {records:?}");
        let topics: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();
        assert!(topics.contains(&"dormant/office-pc/sensor/desk/state"));
        assert!(topics.contains(&"dormant/office-pc/sensor/desk/availability"));
        assert!(topics.contains(&"dormant/office-pc/zone/office/state"));
        assert!(topics.contains(&"dormant/office-pc/display/main/phase"));
    }

    #[test]
    fn snapshot_records_sensor_present_state_and_online_availability() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let state = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/state")
            .unwrap();
        assert_eq!(state.payload, "ON");
        let avail = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/availability")
            .unwrap();
        assert_eq!(avail.payload, "online");
    }

    #[test]
    fn snapshot_records_sensor_unavailable_yields_offline_availability() {
        let cfg = enabled_cfg();
        let mut snap = snapshot_one_of_each();
        snap.sensors[0].state = SensorState::Unavailable;
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let avail = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/availability")
            .unwrap();
        assert_eq!(avail.payload, "offline");
        // State is still OFF (matches Absent) for the fail-toward-blanking
        // policy encoded upstream.
        let state = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/state")
            .unwrap();
        assert_eq!(state.payload, "OFF");
    }

    #[test]
    fn snapshot_records_zone_present_publishes_on() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let zone = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/zone/office/state")
            .unwrap();
        assert_eq!(zone.payload, "ON");
    }

    #[test]
    fn snapshot_records_zone_unresolved_publishes_off() {
        let cfg = enabled_cfg();
        let mut snap = snapshot_one_of_each();
        snap.zones[0].present = None;
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let zone = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/zone/office/state")
            .unwrap();
        assert_eq!(zone.payload, "OFF");
    }

    #[test]
    fn snapshot_records_display_phase_is_lowercase_literal_in_json() {
        let cfg = enabled_cfg();
        let mut snap = snapshot_one_of_each();
        snap.displays[0].1.phase = "GRACE".into();
        let records = snapshot_records(&cfg, &snap, "office-pc");
        let display = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/display/main/phase")
            .unwrap();
        // The engine's `phase` field is the contract's grep-stable
        // literal; we publish it as a JSON object so the HA
        // value_template can extract it. The phase is the
        // engine's exact value (case preserved) — the contract
        // is "lowercase phase literal", which the engine
        // already guarantees upstream.
        let v: serde_json::Value = serde_json::from_str(&display.payload).unwrap();
        assert_eq!(
            v.get("phase").and_then(serde_json::Value::as_str),
            Some("GRACE")
        );
    }

    // ── Event records ──────────────────────────────────────────────────

    #[test]
    fn event_records_sensor_changed_yields_state_and_availability() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::SensorChanged {
            sensor: SensorId("desk".into()),
            state: SensorState::Present,
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 2);
        let state = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/state")
            .unwrap();
        assert_eq!(state.payload, "ON");
        let avail = records
            .iter()
            .find(|r| r.topic == "dormant/office-pc/sensor/desk/availability")
            .unwrap();
        assert_eq!(avail.payload, "online");
    }

    #[test]
    fn event_records_zone_changed_yields_state() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::ZoneChanged {
            zone: ZoneId("office".into()),
            present: false,
            cause: SensorId("desk".into()),
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].topic, "dormant/office-pc/zone/office/state");
        assert_eq!(records[0].payload, "OFF");
    }

    #[test]
    fn event_records_display_phase_yields_phase_literal() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::DisplayPhase {
            display: DisplayId("main".into()),
            phase: "blanking".into(),
            cause: "zone_lost".into(),
        };
        let records = event_records(&cfg, &ev, "office-pc");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].topic, "dormant/office-pc/display/main/phase");
        // Phase is published bare (not wrapped in JSON) so the HA
        // value_template's `{{ value_json.phase }}` works
        // regardless of the engine's choice. The contract
        // pins the value to the engine's phase literal.
        assert_eq!(records[0].payload, "blanking");
    }

    #[test]
    fn event_records_unknown_variant_publishes_nothing() {
        let cfg = enabled_cfg();
        for ev in [
            DaemonEvent::Subscribed,
            DaemonEvent::ConfigReloaded,
            DaemonEvent::Unknown,
            DaemonEvent::WakeRetry {
                display: DisplayId("main".into()),
                attempt: 1,
            },
            DaemonEvent::BlankRecovered {
                display: DisplayId("main".into()),
            },
            DaemonEvent::BlankFailure {
                display: DisplayId("main".into()),
                controller: "kwin-dpms".into(),
                detail: "E_TIMEOUT".into(),
            },
            DaemonEvent::WakeRecovered {
                display: DisplayId("main".into()),
                attempts: 1,
            },
            DaemonEvent::WearSnapshot {
                display: DisplayId("main".into()),
                total_on_hours: 0.0,
                sample_count: 0,
            },
            DaemonEvent::CompensationAdvisory {
                display: DisplayId("main".into()),
                hours_since_long_dwell: 0,
            },
            DaemonEvent::Ownership {
                display: DisplayId("main".into()),
                owned: true,
                observed_input_code: None,
                written_code: None,
                cause: "poll".into(),
                verified: None,
                degraded: false,
            },
        ] {
            let records = event_records(&cfg, &ev, "office-pc");
            assert!(
                records.is_empty(),
                "{ev:?} should not produce publish records, got: {records:?}"
            );
        }
    }

    // ── Collision rule: first-wins ─────────────────────────────────────

    #[test]
    fn collision_two_sensor_ids_same_topic() {
        let mut cfg = enabled_cfg();
        cfg.sensors.insert(
            "desk two".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk2".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        cfg.sensors.insert(
            "desk_two".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/desk3".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        // "desk two" -> "desk_two" (space -> underscore, run-collapsed)
        // "desk_two" -> "desk_two" (already canonical)
        let inv = EntityInventory::from_config(&cfg);
        let collisions = detect_and_warn_collisions(&cfg, &inv);
        assert!(
            collisions.iter().any(|(san, _)| san == "desk_two"),
            "expected desk_two collision, got {collisions:?}"
        );
        // First-wins: discovery emits only the first record.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let topic_count = records
            .iter()
            .filter(|r| r.topic.contains("/sensor_desk_two/"))
            .count();
        assert_eq!(topic_count, 1, "first-wins: only one record per topic");
    }

    #[test]
    fn collision_across_kinds_picks_first() {
        let mut cfg = enabled_cfg();
        cfg.zones.insert(
            "main".into(), // collides with display "main"
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        let inv = EntityInventory::from_config(&cfg);
        // Display "main" came first; zone "main" should be dropped.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let topic_count = records
            .iter()
            .filter(|r| r.topic.contains("/display_main/") || r.topic.contains("/zone_main/"))
            .count();
        assert_eq!(topic_count, 1, "first-wins across kinds");
    }

    #[test]
    fn collision_empty_ids_map_to_unnamed_and_warn() {
        let mut cfg = enabled_cfg();
        cfg.sensors.insert(
            String::new(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://h:1883".into(),
                topic: "dormant/x".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        cfg.zones.insert(
            "   ".into(), // whitespace -> "unnamed"
            ZoneConfig {
                mode: "any".into(),
                members: vec!["desk".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        let inv = EntityInventory::from_config(&cfg);
        let collisions = detect_and_warn_collisions(&cfg, &inv);
        assert!(
            collisions.iter().any(|(san, _)| san == "unnamed"),
            "empty/whitespace ids must collide on 'unnamed', got {collisions:?}"
        );
        // First-wins: sensor "" (config-order first) is kept; the
        // zone is dropped from discovery. The "unnamed" sanitized
        // id shows up in the discovery topic as `sensor_unnamed`.
        let records = discovery_records(&cfg, &inv, "office-pc");
        let unnamed_topics: Vec<&str> = records
            .iter()
            .map(|r| r.topic.as_str())
            .filter(|t| t.contains("unnamed"))
            .collect();
        assert_eq!(unnamed_topics.len(), 1, "first-wins even for 'unnamed'");
    }

    // ── Cross-check: discovery qos/retain are literal 1 / true ─────────

    #[test]
    fn snapshot_records_have_qos1_and_retained() {
        let cfg = enabled_cfg();
        let snap = snapshot_one_of_each();
        for r in snapshot_records(&cfg, &snap, "office-pc") {
            assert_eq!(r.qos, 1);
            assert!(r.retain);
        }
    }

    #[test]
    fn event_records_have_qos1_and_retained() {
        let cfg = enabled_cfg();
        let ev = DaemonEvent::SensorChanged {
            sensor: SensorId("desk".into()),
            state: SensorState::Absent,
        };
        for r in event_records(&cfg, &ev, "office-pc") {
            assert_eq!(r.qos, 1);
            assert!(r.retain);
        }
    }

    // Suppress unused-import warnings for variants only used in tests.
    #[allow(dead_code)]
    fn _unused() {
        let _ = Duration::from_secs(0);
    }
}
