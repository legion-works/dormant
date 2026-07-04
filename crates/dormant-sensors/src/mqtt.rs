//! MQTT sensor source — subscribes to `Zigbee2MQTT` (and other) topics and
//! parses occupancy/availability payloads into [`PresenceEvent`]s.
//!
//! ## Architecture
//!
//! One [`MqttSource`] per distinct `broker_url` — it multiplexes all sensors
//! sharing that broker into a single rumqttc connection.  Each sensor gets its
//! own topic subscription plus an availability topic (`<topic>/availability`).
//!
//! ## Fail-safe behaviour
//!
//! - Broker disconnect → emit [`SensorState::Unavailable`] for **all** owned
//!   sensors once per outage, then reconnect with exponential backoff.
//! - On reconnect, the client uses `clean_session = true` and explicitly
//!   re-subscribes (simpler to reason about than relying on broker-stored
//!   subscriptions).
//! - Availability payload `"offline"` → [`SensorState::Unavailable`] for that
//!   sensor.  `"online"` → **no event** (the last real occupancy publish
//!   remains authoritative; emitting `Present` would defeat absence detection,
//!   emitting `Absent` would violate fail-safe presence — doing nothing lets
//!   the stale-sensor sweeper or next real publish handle it).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::config::schema::MqttSensorCfg;
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Minimum reconnect backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);

/// Maximum reconnect backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Jitter fraction (±20%).
const JITTER_FRACTION: f64 = 0.20;

/// Global counter for unique client IDs.
static CLIENT_COUNTER: AtomicU16 = AtomicU16::new(0);

// ── MqttSource ─────────────────────────────────────────────────────────────────

/// An MQTT sensor source that multiplexes multiple sensors on one broker
/// connection.
pub struct MqttSource {
    /// Broker URL (e.g. `tcp://localhost:1883`).
    broker_url: String,
    /// Per-sensor configuration, paired with its stable [`SensorId`].
    sensors: Vec<(SensorId, MqttSensorCfg)>,
}

impl MqttSource {
    /// Create a new `MqttSource` for the given broker and sensor list.
    ///
    /// All sensors in `sensors` must share the same `broker_url` — callers
    /// (the registry) are responsible for grouping.
    #[must_use]
    pub fn new(broker_url: String, sensors: Vec<(SensorId, MqttSensorCfg)>) -> Self {
        Self {
            broker_url,
            sensors,
        }
    }

    /// Build a unique MQTT client ID: `dormant-<hostname>-<counter>`.
    fn client_id() -> String {
        let hostname = gethostname::gethostname().to_string_lossy().to_string();
        let n = CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("dormant-{hostname}-{n}")
    }

    /// Collect all topics this source subscribes to (sensor + availability).
    fn all_topics(&self) -> Vec<String> {
        let mut topics: Vec<String> = Vec::with_capacity(self.sensors.len() * 2);
        for (_, cfg) in &self.sensors {
            topics.push(cfg.topic.clone());
            topics.push(availability_topic(&cfg.topic));
        }
        topics
    }

    /// Emit [`SensorState::Unavailable`] for every owned sensor.
    async fn emit_unavailable_all(&self, tx: &mpsc::Sender<PresenceEvent>) {
        let now = Timestamp::now();
        for (sensor_id, _) in &self.sensors {
            let event = PresenceEvent::new(sensor_id.clone(), SensorState::Unavailable, now);
            if tx.send(event).await.is_err() {
                // Receiver dropped — we are shutting down.
                return;
            }
        }
    }

    /// Create a fresh MQTT connection, subscribe to all topics, and return
    /// the client+eventloop pair.
    ///
    /// `broker_url` is expected in the form `host:port` (e.g. `localhost:1883`)
    /// or `tcp://host:port`.
    async fn connect(
        broker_url: &str,
        client_id: &str,
        topics: &[String],
    ) -> (AsyncClient, EventLoop) {
        let (host, port) = parse_broker_url(broker_url);
        let mut mqttopts = MqttOptions::new(client_id, host, port);
        mqttopts.set_clean_session(true);
        let (client, eventloop) = AsyncClient::new(mqttopts, 100);
        for topic in topics {
            let _ = client.subscribe(topic, QoS::AtLeastOnce).await;
        }
        (client, eventloop)
    }

    /// Map a topic string back to the sensor config that owns it (or its
    /// availability topic).
    fn sensor_for_topic(&self, topic: &str) -> Option<&(SensorId, MqttSensorCfg)> {
        self.sensors
            .iter()
            .find(|(_, cfg)| cfg.topic == topic || availability_topic(&cfg.topic) == topic)
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl SensorSource for MqttSource {
    fn source_id(&self) -> &str {
        &self.broker_url
    }

    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<PresenceEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let client_id = Self::client_id();
        let topics = self.all_topics();

        // ── Outer reconnect loop ───────────────────────────────────────────
        let mut backoff = BACKOFF_MIN;
        let mut warned_topics: HashSet<String> = HashSet::new();
        let mut outage_reported = false;

        // We hold the current client+eventloop pair in these variables.
        // On reconnect we drop both and create a fresh pair.
        let (mut client, mut eventloop) =
            Self::connect(&self.broker_url, &client_id, &topics).await;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    info!("mqtt source '{}' cancelled, disconnecting", self.broker_url);
                    let _ = client.disconnect().await;
                    return Ok(());
                }
                event = eventloop.poll() => {
                    match event {
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            let topic = publish.topic.clone();
                            let payload = publish.payload.to_vec();

                            // Check availability topic first.
                            if let Some(_avail_topic) = self.sensors.iter().find_map(|(_, cfg)| {
                                let at = availability_topic(&cfg.topic);
                                if at == topic { Some(at) } else { None }
                            }) {
                            // Availability payload.
                            let state = parse_availability(&payload);
                            if let Some(sensor) = self.sensor_for_topic(&topic)
                                && let Some(state) = state
                            {
                                let event = PresenceEvent::new(
                                    sensor.0.clone(),
                                    state,
                                    Timestamp::now(),
                                );
                                let _ = tx.send(event).await;
                            }
                            // "online" → no event (see module docs).
                                continue;
                            }

                            // Regular sensor topic.
                            if let Some(sensor) = self.sensor_for_topic(&topic) {
                                let (sensor_id, cfg) = sensor;
                                match parse_payload(cfg, &payload) {
                                    Some(state) => {
                                        let event = PresenceEvent::new(
                                            sensor_id.clone(),
                                            state,
                                            Timestamp::now(),
                                        );
                                        if tx.send(event).await.is_err() {
                                            return Ok(());
                                        }
                                    }
                                    None => {
                                        if warned_topics.insert(topic.clone()) {
                                            warn!(
                                                "mqtt: unparseable payload on '{}' (first occurrence)",
                                                topic,
                                            );
                                        } else {
                                            debug!(
                                                "mqtt: unparseable payload on '{}' (suppressed)",
                                                topic,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            // Clean session = true means we must re-subscribe
                            // on every ConnAck.
                            info!("mqtt: reconnected to '{}', re-subscribing", self.broker_url);
                            for topic in &topics {
                                if let Err(e) = client.subscribe(topic, QoS::AtLeastOnce).await {
                                    warn!(
                                        "mqtt: subscribe failed for '{}': {e}",
                                        topic,
                                    );
                                }
                            }
                            backoff = BACKOFF_MIN;
                            outage_reported = false;
                        }
                        Ok(Event::Incoming(Packet::Disconnect)) => {
                            debug!("mqtt: broker-initiated disconnect from '{}'", self.broker_url);
                        }
                        Ok(_) => {
                            // Ignore other incoming packets (PingResp, SubAck, etc.).
                        }
                        Err(e) => {
                            warn!(
                                "mqtt: connection error on '{}': {e}",
                                self.broker_url,
                            );
                            if !outage_reported {
                                self.emit_unavailable_all(&tx).await;
                                outage_reported = true;
                            }
                            sleep(backoff).await;
                            backoff = next_backoff(backoff);
                            // Reconnect: drop old pair, create new.
                            let new_pair = Self::connect(
                                &self.broker_url, &client_id, &topics,
                            ).await;
                            client = new_pair.0;
                            eventloop = new_pair.1;
                        }
                    }
                }
            }
        }
    }
}

// ── Payload parsing ───────────────────────────────────────────────────────────

/// Parse an MQTT payload into a [`SensorState`] based on the sensor config.
///
/// Returns `None` when the payload cannot be interpreted (malformed JSON,
/// missing field, unrecognised literal value).
///
/// # Resolution order
///
/// 1. If `cfg.payload_on` / `cfg.payload_off` are set, the payload is compared
///    as trimmed bytes against those literals.
/// 2. Otherwise the payload is parsed as JSON, the configured `field` (JSON
///    pointer) is resolved, and the value is interpreted as a boolean.
/// 3. Non-bool string values `"ON"`, `"OFF"`, `"true"`, `"false"` (Zigbee2MQTT
///    variants) are also accepted.
#[doc(hidden)] // pub(crate) but doc-hidden for rustdoc
#[must_use]
pub fn parse_payload(cfg: &MqttSensorCfg, payload: &[u8]) -> Option<SensorState> {
    // Literal payload match.
    if let Some(on) = &cfg.payload_on {
        let trimmed = String::from_utf8_lossy(payload).trim().to_string();
        if trimmed == *on {
            return Some(SensorState::Present);
        }
    }
    if let Some(off) = &cfg.payload_off {
        let trimmed = String::from_utf8_lossy(payload).trim().to_string();
        if trimmed == *off {
            return Some(SensorState::Absent);
        }
    }
    if cfg.payload_on.is_some() || cfg.payload_off.is_some() {
        // Literal mode configured but no match — don't fall through to JSON.
        return None;
    }

    // JSON parsing.
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;

    // Resolve JSON pointer.
    let field_value = if cfg.field == "/" || cfg.field.is_empty() {
        &value
    } else {
        value.pointer(&cfg.field)?
    };

    match field_value {
        serde_json::Value::Bool(b) => {
            if *b {
                Some(SensorState::Present)
            } else {
                Some(SensorState::Absent)
            }
        }
        serde_json::Value::String(s) => match s.as_str() {
            "ON" | "true" | "on" | "True" | "TRUE" => Some(SensorState::Present),
            "OFF" | "false" | "off" | "False" | "FALSE" => Some(SensorState::Absent),
            _ => None,
        },
        _ => None,
    }
}

/// Parse an availability payload into a [`SensorState`].
///
/// Handles both plain-text `"online"` / `"offline"` and JSON
/// `{"state":"online"}` / `{"state":"offline"}` (`Zigbee2MQTT` variants).
///
/// Returns `Some(SensorState::Unavailable)` for offline, `None` for online
/// (no event — see module-level docs), and `None` for unrecognised payloads.
fn parse_availability(payload: &[u8]) -> Option<SensorState> {
    // Try plain text first.
    let text = String::from_utf8_lossy(payload).trim().to_string();
    match text.as_str() {
        "offline" => return Some(SensorState::Unavailable),
        "online" => return None,
        _ => {}
    }

    // Try JSON.
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload)
        && let Some(state) = value.get("state").and_then(|v| v.as_str())
    {
        match state {
            "offline" => return Some(SensorState::Unavailable),
            "online" => return None,
            _ => {}
        }
    }

    None
}

/// Build the availability topic for a sensor topic.
///
/// `Zigbee2MQTT` convention: `<topic>/availability`.
#[must_use]
pub fn availability_topic(topic: &str) -> String {
    format!("{topic}/availability")
}

// ── Backoff helpers ───────────────────────────────────────────────────────────

/// Compute the next backoff duration with capped exponential growth and ±20%
/// jitter.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn next_backoff(current: Duration) -> Duration {
    let next = current.mul_f64(2.0).min(BACKOFF_MAX);
    if JITTER_FRACTION <= 0.0 {
        return next.max(BACKOFF_MIN).min(BACKOFF_MAX);
    }
    // Jitter: ±20% of the next backoff value.
    let jitter_range_ms = next.mul_f64(JITTER_FRACTION).as_millis();
    if jitter_range_ms == 0 {
        return next.max(BACKOFF_MIN).min(BACKOFF_MAX);
    }
    let offset_ms = ((fastrand::f64() * 2.0 - 1.0) * jitter_range_ms as f64) as i64;
    let result = if offset_ms >= 0 {
        next.saturating_add(Duration::from_millis(offset_ms as u64))
    } else {
        next.saturating_sub(Duration::from_millis((-offset_ms) as u64))
    };
    result.max(BACKOFF_MIN).min(BACKOFF_MAX)
}

/// Parse a broker URL into (host, port).
///
/// Accepts `host:port`, `tcp://host:port`, `mqtt://host:port`.
fn parse_broker_url(url: &str) -> (&str, u16) {
    // Strip tcp:// or mqtt:// prefix.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use dormant_core::config::schema::SensorKind;
    use std::time::Duration;

    // ── Fixture helpers ────────────────────────────────────────────────────

    fn make_cfg(field: &str) -> MqttSensorCfg {
        MqttSensorCfg {
            broker_url: "tcp://localhost:1883".into(),
            topic: "test/sensor".into(),
            field: field.into(),
            payload_on: None,
            payload_off: None,
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
        }
    }

    fn make_cfg_literal(on: &str, off: &str) -> MqttSensorCfg {
        MqttSensorCfg {
            broker_url: "tcp://localhost:1883".into(),
            topic: "test/sensor".into(),
            field: "/occupancy".into(),
            payload_on: Some(on.into()),
            payload_off: Some(off.into()),
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
        }
    }

    // ── parse_payload: JSON fixtures ───────────────────────────────────────

    #[test]
    fn snzb06p_occupancy_true_parses_present() {
        let payload = include_bytes!("../fixtures/z2m_snzb06p.json");
        let cfg = make_cfg("/occupancy");
        assert_eq!(parse_payload(&cfg, payload), Some(SensorState::Present));
    }

    #[test]
    fn snzb03_occupancy_false_parses_absent() {
        let payload = include_bytes!("../fixtures/z2m_snzb03_pir.json");
        let cfg = make_cfg("/occupancy");
        assert_eq!(parse_payload(&cfg, payload), Some(SensorState::Absent));
    }

    #[test]
    fn availability_offline_maps_unavailable_plain() {
        let payload = b"offline";
        assert_eq!(parse_availability(payload), Some(SensorState::Unavailable),);
    }

    #[test]
    fn availability_offline_maps_unavailable_json() {
        let payload = include_bytes!("../fixtures/z2m_availability.json");
        assert_eq!(parse_availability(payload), Some(SensorState::Unavailable),);
    }

    #[test]
    fn availability_online_returns_none() {
        assert_eq!(parse_availability(b"online"), None);
        assert_eq!(parse_availability(br#"{"state":"online"}"#), None,);
    }

    // ── parse_payload: literal match ───────────────────────────────────────

    #[test]
    fn literal_payload_match_for_non_json() {
        let cfg = make_cfg_literal("ON", "OFF");
        assert_eq!(parse_payload(&cfg, b"ON"), Some(SensorState::Present),);
        assert_eq!(parse_payload(&cfg, b"OFF"), Some(SensorState::Absent),);
        // Whitespace tolerance.
        assert_eq!(parse_payload(&cfg, b"  ON  "), Some(SensorState::Present),);
        // Non-matching literal returns None.
        assert_eq!(parse_payload(&cfg, b"UNKNOWN"), None);
    }

    // ── parse_payload: error cases ─────────────────────────────────────────

    #[test]
    fn malformed_json_and_missing_field_yield_none() {
        let cfg = make_cfg("/occupancy");
        assert_eq!(parse_payload(&cfg, b"not json"), None);
        assert_eq!(parse_payload(&cfg, b"{}"), None);
    }

    #[test]
    fn custom_field_pointer() {
        let cfg = MqttSensorCfg {
            broker_url: "tcp://localhost:1883".into(),
            topic: "test/sensor".into(),
            field: "/presence".into(),
            payload_on: None,
            payload_off: None,
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
        };
        let payload = br#"{"presence":true,"temperature":22.5}"#;
        assert_eq!(parse_payload(&cfg, payload), Some(SensorState::Present));

        let payload = br#"{"presence":false}"#;
        assert_eq!(parse_payload(&cfg, payload), Some(SensorState::Absent));
    }

    #[test]
    fn z2m_string_variants_accepted() {
        let cfg = make_cfg("/occupancy");
        assert_eq!(
            parse_payload(&cfg, br#"{"occupancy":"ON"}"#),
            Some(SensorState::Present),
        );
        assert_eq!(
            parse_payload(&cfg, br#"{"occupancy":"OFF"}"#),
            Some(SensorState::Absent),
        );
        assert_eq!(
            parse_payload(&cfg, br#"{"occupancy":"true"}"#),
            Some(SensorState::Present),
        );
        assert_eq!(
            parse_payload(&cfg, br#"{"occupancy":"false"}"#),
            Some(SensorState::Absent),
        );
    }

    // ── availability_topic ─────────────────────────────────────────────────

    #[test]
    fn availability_topic_construction() {
        assert_eq!(
            availability_topic("sensors/desk"),
            "sensors/desk/availability",
        );
        assert_eq!(
            availability_topic("zigbee2mqtt/0x00158d0003c3a1b2"),
            "zigbee2mqtt/0x00158d0003c3a1b2/availability",
        );
    }

    // ── Backoff ────────────────────────────────────────────────────────────

    #[test]
    fn backoff_stays_within_bounds() {
        let mut b = BACKOFF_MIN;
        for _ in 0..20 {
            b = next_backoff(b);
            assert!(
                b >= BACKOFF_MIN && b <= BACKOFF_MAX,
                "backoff {b:?} out of bounds [{BACKOFF_MIN:?}, {BACKOFF_MAX:?}]",
            );
        }
    }

    #[test]
    fn backoff_eventually_caps() {
        let mut b = BACKOFF_MIN;
        for _ in 0..10 {
            b = next_backoff(b);
        }
        // After enough doublings it should be at or near the cap.
        assert!(
            b >= Duration::from_secs(20),
            "backoff {b:?} should be near cap"
        );
    }

    // ── MqttSource construction ────────────────────────────────────────────

    #[test]
    fn source_id_returns_broker_url() {
        let source = MqttSource::new(
            "tcp://mqtt.local:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
        );
        assert_eq!(source.source_id(), "tcp://mqtt.local:1883");
    }

    #[test]
    fn all_topics_includes_availability() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(
                SensorId("desk".into()),
                MqttSensorCfg {
                    broker_url: "tcp://localhost:1883".into(),
                    topic: "sensors/desk".into(),
                    field: "/occupancy".into(),
                    payload_on: None,
                    payload_off: None,
                    kind: SensorKind::Presence,
                    hold_time: None,
                    stale_timeout: None,
                },
            )],
        );
        let topics = source.all_topics();
        assert!(topics.contains(&"sensors/desk".to_string()));
        assert!(topics.contains(&"sensors/desk/availability".to_string()));
    }

    #[test]
    fn sensor_for_topic_finds_by_topic() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(
                SensorId("desk".into()),
                MqttSensorCfg {
                    broker_url: "tcp://localhost:1883".into(),
                    topic: "sensors/desk".into(),
                    field: "/occupancy".into(),
                    payload_on: None,
                    payload_off: None,
                    kind: SensorKind::Presence,
                    hold_time: None,
                    stale_timeout: None,
                },
            )],
        );
        assert!(source.sensor_for_topic("sensors/desk").is_some());
        assert!(
            source
                .sensor_for_topic("sensors/desk/availability")
                .is_some()
        );
        assert!(source.sensor_for_topic("unknown").is_none());
    }
}
