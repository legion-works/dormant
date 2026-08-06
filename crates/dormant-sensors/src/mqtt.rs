//! MQTT sensor source — subscribes to `Zigbee2MQTT` (and other) topics and
//! parses occupancy/availability payloads into [`PresenceEvent`]s.
//!
//! ## Architecture
//!
//! One [`MqttSource`] per distinct `broker_url` — it multiplexes all sensors
//! sharing that broker into a single rumqttc connection.  Each sensor gets its
//! own topic subscription plus an availability topic — by default derived as
//! `<topic>/availability` (see [`availability_topic`]), or an explicit
//! `availability_topic` override from config.
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
//!   emitting `Absent` would violate fail-safe presence).
//! - Retained availability publishes are accepted the same as live ones: this
//!   module has no way to see a retained publish's real age, so a broker
//!   replaying a stale retained `"offline"`/`"online"` on (re)subscribe resets
//!   the stale-sensor sweep's last-seen clock (`dormant_core::rules`) exactly
//!   as a fresh publish would.  That sweep only ever pushes a sensor *into*
//!   `Unavailable` when it goes quiet past its `stale_timeout` — once a
//!   sensor is `Unavailable` the sweep skips it, so nothing sweeps it back;
//!   only a new state publish (occupancy, or a fresh availability payload)
//!   rescues it.
//!
//! ## Out of M1 scope
//!
//! - MQTT authentication (username/password, TLS client certs).
//! - Wildcard topic subscriptions (`+`, `#`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dormant_core::config::schema::{MqttCredential, MqttSensorCfg};
use dormant_core::mqtt::parse_broker_url;
use dormant_core::rules::ControlMsg;
use dormant_core::traits::SensorSource;
use dormant_core::types::{
    PresenceEvent, SensorAvailabilityEvent, SensorId, SensorState, Timestamp,
};
use rumqttc::mqttbytes::v4::SubscribeReasonCode;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Outgoing, Packet, QoS};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff;

// ── Constants ──────────────────────────────────────────────────────────────────

/// Minimum reconnect backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);

/// Maximum reconnect backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Jitter fraction (±20%).
const JITTER_FRACTION: f64 = 0.20;

/// Global counter for unique client IDs.
static CLIENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Extra capacity beyond subscription count for the rumqttc channel.
const CAP_HEADROOM: usize = 16;

// ── Topic-index types ──────────────────────────────────────────────────────────

/// A single sensor binding: its stable id and its config.
#[derive(Debug, Clone)]
struct SensorBinding {
    id: SensorId,
    cfg: MqttSensorCfg,
}

/// Pre-computed topic → sensor-bindings map.
///
/// Built at construction so that a single publish on a shared topic fans out to
/// every sensor subscribed to that topic (each with its own field pointer).
type TopicMap = HashMap<String, Vec<SensorBinding>>;

#[async_trait]
trait MqttEventLoop: Send {
    async fn poll(&mut self) -> anyhow::Result<Event>;
    async fn subscribe(&mut self, topic: &str, qos: QoS) -> anyhow::Result<()>;
    async fn disconnect(&mut self) -> anyhow::Result<()>;
    async fn reconnect(&mut self) -> anyhow::Result<()>;
}

struct RumqttEventLoop {
    client: AsyncClient,
    eventloop: EventLoop,
    broker_url: String,
    client_id: String,
    credential: Option<MqttCredential>,
    topic_count: usize,
}

#[async_trait]
impl MqttEventLoop for RumqttEventLoop {
    async fn poll(&mut self) -> anyhow::Result<Event> {
        self.eventloop.poll().await.map_err(Into::into)
    }

    async fn subscribe(&mut self, topic: &str, qos: QoS) -> anyhow::Result<()> {
        self.client.subscribe(topic, qos).await.map_err(Into::into)
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.client.disconnect().await.map_err(Into::into)
    }

    async fn reconnect(&mut self) -> anyhow::Result<()> {
        let (host, port) = parse_broker_url(&self.broker_url)
            .map_err(|e| anyhow::anyhow!("invalid mqtt broker_url {:?}: {e}", self.broker_url))?;
        let mut mqttopts = MqttOptions::new(&self.client_id, host, port);
        mqttopts.set_clean_session(true);
        if let Some(cred) = &self.credential {
            mqttopts.set_credentials(cred.username.clone(), cred.password.clone());
        }
        let (client, eventloop) = AsyncClient::new(mqttopts, self.topic_count + CAP_HEADROOM);
        self.client = client;
        self.eventloop = eventloop;
        Ok(())
    }
}

/// Connection observations exposed only to external integration tests.
///
/// All variants are constructed only inside `#[cfg(feature = "test-util")]` blocks,
/// so the enum itself is safe to keep ungated (it is uninhabited in non-test-util
/// builds, causing no dead-code warnings).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MqttLifecycle {
    /// The broker accepted the MQTT connection.
    Connected,
    /// N topic subscriptions were queued to the client request channel.
    ///
    /// Emitted from inside `subscribe_topics` after each batch is queued, before
    /// the broker has acknowledged or rejected any of them. Fires on every
    /// `subscribe_topics` call regardless of caller, so it observes batches
    /// issued both from `connect()` and from the [`ConnAck`](rumqttc::ConnAck) handler.
    SubscribeQueued {
        /// Number of topic subscriptions queued.
        count: usize,
    },
    /// Every queued topic subscription was acknowledged by the broker.
    Subscribed,
}

// ── MqttSource ─────────────────────────────────────────────────────────────────

/// An MQTT sensor source that multiplexes multiple sensors on one broker
/// connection.
pub struct MqttSource {
    /// Broker URL (e.g. `tcp://localhost:1883`).
    broker_url: String,
    /// Topic → sensor bindings (sensor topics + availability topics).
    topic_map: TopicMap,
    /// The subset of `topic_map` keys that are availability topics (derived
    /// or overridden) — routes publishes to the availability branch instead
    /// of matching on a hardcoded `/availability` suffix (spec F1).
    availability_topics: HashSet<String>,
    /// Flat list of all topics to subscribe to.
    topics: Vec<String>,
    /// Optional per-broker MQTT credentials (username + password).
    credential: Option<MqttCredential>,
    /// Test-only protocol lifecycle observations.
    #[cfg(feature = "test-util")]
    lifecycle_tx: Option<mpsc::UnboundedSender<MqttLifecycle>>,
}

impl MqttSource {
    /// Create a new `MqttSource` for the given broker, sensor list, and
    /// optional credentials.
    ///
    /// All sensors in `sensors` must share the same `broker_url` — callers
    /// (the registry) are responsible for grouping.
    #[must_use]
    pub fn new(
        broker_url: String,
        sensors: Vec<(SensorId, MqttSensorCfg)>,
        credential: Option<MqttCredential>,
    ) -> Self {
        let mut topic_map: TopicMap = HashMap::new();
        let mut availability_topics: HashSet<String> = HashSet::new();
        let mut topics: Vec<String> = Vec::with_capacity(sensors.len() * 2);

        for (id, cfg) in sensors {
            let binding = SensorBinding {
                id,
                cfg: cfg.clone(),
            };

            // Sensor topic.
            let sensor_topic = cfg.topic.clone();
            topic_map
                .entry(sensor_topic.clone())
                .or_default()
                .push(binding.clone());
            topics.push(sensor_topic);

            // Availability topic — overridable per-binding, otherwise derived
            // from the sensor topic (`Zigbee2MQTT` convention).
            let avail_topic = cfg
                .availability_topic
                .clone()
                .unwrap_or_else(|| availability_topic(&cfg.topic));
            topic_map
                .entry(avail_topic.clone())
                .or_default()
                .push(binding);
            availability_topics.insert(avail_topic.clone());
            topics.push(avail_topic);
        }

        Self {
            broker_url,
            topic_map,
            availability_topics,
            topics,
            credential,
            #[cfg(feature = "test-util")]
            lifecycle_tx: None,
        }
    }

    /// Attach a test-only receiver for broker protocol observations.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn with_lifecycle_sender(
        mut self,
        lifecycle_tx: mpsc::UnboundedSender<MqttLifecycle>,
    ) -> Self {
        self.lifecycle_tx = Some(lifecycle_tx);
        self
    }

    /// Test-only access to the optional credential.
    #[cfg(test)]
    #[must_use]
    pub fn credential(&self) -> Option<&MqttCredential> {
        self.credential.as_ref()
    }

    /// Build a unique MQTT client ID for concurrent daemon and test processes.
    fn client_id() -> String {
        let hostname = gethostname::gethostname().to_string_lossy().to_string();
        let n = CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the epoch")
            .as_nanos();
        format!("dormant-{hostname}-{}-{n}-{suffix}", std::process::id())
    }

    /// Emit [`SensorState::Unavailable`] for every owned sensor.
    async fn emit_unavailable_all(&self, tx: &mpsc::Sender<PresenceEvent>) {
        // Deduplicate by sensor id — a sensor appears in the topic map under
        // both its sensor topic and its availability topic.
        let mut seen: HashSet<&SensorId> = HashSet::new();
        let mut ids: Vec<SensorId> = Vec::new();
        for bindings in self.topic_map.values() {
            for binding in bindings {
                if seen.insert(&binding.id) {
                    ids.push(binding.id.clone());
                }
            }
        }
        backoff::emit_unavailable_all(&ids, tx).await;
    }

    /// Queue subscriptions for every topic and return the number accepted by
    /// the client request channel.
    async fn subscribe_topics(
        eventloop: &mut dyn MqttEventLoop,
        topics: &[String],
        #[allow(unused_variables)] lifecycle_tx: Option<&mpsc::UnboundedSender<MqttLifecycle>>,
    ) -> usize {
        let mut queued = 0;
        for topic in topics {
            match eventloop.subscribe(topic, QoS::AtLeastOnce).await {
                Ok(()) => queued += 1,
                Err(e) => warn!("mqtt: initial subscribe failed for '{topic}': {e}"),
            }
        }
        #[cfg(feature = "test-util")]
        if let Some(tx) = lifecycle_tx {
            let _ = tx.send(MqttLifecycle::SubscribeQueued { count: queued });
        }
        queued
    }

    fn subscription_ack_succeeded(return_codes: &[SubscribeReasonCode]) -> bool {
        return_codes
            .iter()
            .all(|code| matches!(code, SubscribeReasonCode::Success(_)))
    }

    /// Create a fresh MQTT connection behind the event-loop seam.
    ///
    /// `broker_url` is expected in the form `host:port` (e.g. `localhost:1883`)
    /// or `tcp://host:port`. A malformed URL surfaces as an `anyhow::Error`
    /// so the caller can fail fast rather than connect to a garbage host.
    /// Subscriptions are NOT issued here — [`ConnAck`](rumqttc::ConnAck) is
    /// the sole subscription site so that first-session and reconnect sessions
    /// behave identically.
    fn connect(
        broker_url: &str,
        client_id: &str,
        topic_count: usize,
        credential: Option<&MqttCredential>,
    ) -> anyhow::Result<Box<dyn MqttEventLoop>> {
        let (host, port) = parse_broker_url(broker_url)
            .map_err(|e| anyhow::anyhow!("invalid mqtt broker_url {broker_url:?}: {e}"))?;
        let mut mqttopts = MqttOptions::new(client_id, host, port);
        mqttopts.set_clean_session(true);
        if let Some(cred) = credential {
            mqttopts.set_credentials(cred.username.clone(), cred.password.clone());
        }
        let cap = topic_count + CAP_HEADROOM;
        let (client, eventloop) = AsyncClient::new(mqttopts, cap);
        Ok(Box::new(RumqttEventLoop {
            client,
            eventloop,
            broker_url: broker_url.to_owned(),
            client_id: client_id.to_owned(),
            credential: credential.cloned(),
            topic_count,
        }))
    }

    /// Dispatch a publish on a sensor topic: parse each matching binding's
    /// payload and return the resulting events.
    ///
    /// Returns `(presence_events, availability_events)`. Availability
    /// events are the LWT edges the rules engine needs to gate the stale
    /// sweep; presence events are the regular occupancy publishes. An
    /// `offline` availability payload still surfaces as a
    /// `PresenceEvent::Unavailable` so the fail-safe presence path takes
    /// over immediately (the existing pre-#136 behavior) — the
    /// availability event is the additional signal that ALSO clears the
    /// `availability_online` set in the rules engine.
    fn dispatch_publish(
        &self,
        topic: &str,
        payload: &[u8],
        warned: &mut HashSet<String>,
        warned_availability: &mut HashSet<(String, String)>,
    ) -> (Vec<PresenceEvent>, Vec<SensorAvailabilityEvent>) {
        let now = Timestamp::now();
        let mut events = Vec::new();
        let mut availability = Vec::new();

        // Check availability topic first — routed via the precomputed set
        // (spec F1), not a hardcoded `/availability` suffix, so an
        // `availability_topic` override is honoured.
        if self.availability_topics.contains(topic) {
            if let Some(bindings) = self.topic_map.get(topic) {
                for binding in bindings {
                    match parse_availability(
                        payload,
                        &binding.cfg.availability_payload_online,
                        &binding.cfg.availability_payload_offline,
                    ) {
                        AvailabilityParse::Offline => {
                            events.push(PresenceEvent::new(
                                binding.id.clone(),
                                SensorState::Unavailable,
                                now,
                            ));
                            availability.push(SensorAvailabilityEvent::new(
                                binding.id.clone(),
                                false,
                                now,
                            ));
                        }
                        AvailabilityParse::Online => {
                            // Issue #136: emit a typed availability edge so
                            // the rules engine's stale sweep leaves this
                            // sensor alone on topic silence. We do NOT
                            // touch presence state (an "online" frame is a
                            // reachability claim, not a state publish).
                            availability.push(SensorAvailabilityEvent::new(
                                binding.id.clone(),
                                true,
                                now,
                            ));
                        }
                        AvailabilityParse::Unrecognized => {
                            let id = binding.id.0.clone();
                            let key = (topic.to_string(), id.clone());
                            if warned_availability.insert(key) {
                                warn!(
                                    "mqtt: unrecognised availability payload on '{topic}' for sensor '{id}' (first occurrence)"
                                );
                            } else {
                                debug!(
                                    "mqtt: unrecognised availability payload on '{topic}' for sensor '{id}' (suppressed)"
                                );
                            }
                        }
                    }
                }
            }
            return (events, availability);
        }

        // Regular sensor topic.
        let Some(bindings) = self.topic_map.get(topic) else {
            return (events, availability);
        };

        for binding in bindings {
            match parse_payload(&binding.cfg, payload) {
                Some(state) => {
                    events.push(PresenceEvent::new(binding.id.clone(), state, now));
                }
                None => {
                    if warned.insert(topic.to_string()) {
                        warn!("mqtt: unparsable payload on '{}' (first occurrence)", topic,);
                    } else {
                        debug!("mqtt: unparsable payload on '{}' (suppressed)", topic);
                    }
                }
            }
        }

        (events, availability)
    }
}

#[allow(clippy::too_many_lines)]
impl MqttSource {
    async fn run_source(
        self: Box<Self>,
        tx: mpsc::Sender<PresenceEvent>,
        ctl_tx: mpsc::Sender<ControlMsg>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let client_id = Self::client_id();
        let eventloop = Self::connect(
            &self.broker_url,
            &client_id,
            self.topics.len(),
            self.credential.as_ref(),
        )?;
        self.run_with_event_loop(eventloop, tx, ctl_tx, cancel)
            .await
    }

    async fn run_with_event_loop(
        self: Box<Self>,
        mut eventloop: Box<dyn MqttEventLoop>,
        tx: mpsc::Sender<PresenceEvent>,
        ctl_tx: mpsc::Sender<ControlMsg>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let topics = self.topics.clone();

        // ── Outer reconnect loop ───────────────────────────────────────────
        let mut backoff = BACKOFF_MIN;
        let mut warned_topics: HashSet<String> = HashSet::new();
        let mut warned_availability: HashSet<(String, String)> = HashSet::new();
        let mut outage_reported = false;

        let mut pending_subacks: HashMap<u16, String> = HashMap::new();
        let mut acknowledged_subscriptions = 0;
        let mut outgoing_subscriptions = 0;
        let mut queued_subscriptions = 0;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    info!("mqtt source '{}' cancelled, disconnecting", self.broker_url);
                    let _ = eventloop.disconnect().await;
                    return Ok(());
                }
                event = eventloop.poll() => {
                    match event {
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            let topic = publish.topic.clone();
                            let payload = publish.payload.to_vec();
                            if publish.retain {
                                debug!("mqtt: retained publish on '{}' dispatched", topic);
                            }

                            let (events, availability) = self.dispatch_publish(
                                &topic, &payload, &mut warned_topics, &mut warned_availability,
                            );
                            for event in events {
                                if tx.send(event).await.is_err() {
                                    return Ok(());
                                }
                            }
                            for avail in availability {
                                if ctl_tx
                                    .send(ControlMsg::SensorAvailability(avail))
                                    .await
                                    .is_err()
                                {
                                    return Ok(());
                                }
                            }
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            pending_subacks.clear();
                            acknowledged_subscriptions = 0;
                            outgoing_subscriptions = 0;
                            #[cfg(feature = "test-util")]
                            if let Some(lifecycle_tx) = &self.lifecycle_tx {
                                let _ = lifecycle_tx.send(MqttLifecycle::Connected);
                            }
                            info!("mqtt: connected to '{}', subscribing", self.broker_url);
                            #[cfg(feature = "test-util")]
                            let lifecycle = self.lifecycle_tx.as_ref();
                            #[cfg(not(feature = "test-util"))]
                            let lifecycle: Option<&mpsc::UnboundedSender<MqttLifecycle>> = None;
                            queued_subscriptions =
                                Self::subscribe_topics(eventloop.as_mut(), &topics, lifecycle).await;
                            backoff = BACKOFF_MIN;
                            outage_reported = false;
                        }
                        Ok(Event::Outgoing(Outgoing::Subscribe(pkid))) => {
                            if let Some(topic) = topics.get(outgoing_subscriptions) {
                                pending_subacks.insert(pkid, topic.clone());
                                outgoing_subscriptions += 1;
                            }
                        }
                        Ok(Event::Incoming(Packet::SubAck(suback))) => {
                            if let Some(topic) = pending_subacks.remove(&suback.pkid) {
                                if Self::subscription_ack_succeeded(&suback.return_codes) {
                                    acknowledged_subscriptions += 1;
                                    if acknowledged_subscriptions == queued_subscriptions {
                                        #[cfg(feature = "test-util")]
                                        if let Some(lifecycle_tx) = &self.lifecycle_tx {
                                            let _ = lifecycle_tx.send(MqttLifecycle::Subscribed);
                                        }
                                    }
                                } else {
                                    warn!(
                                        "mqtt: subscription rejected for '{topic}' (pkid {}): {:?}",
                                        suback.pkid, suback.return_codes,
                                    );
                                }
                            }
                        }
                        Ok(Event::Incoming(Packet::Disconnect)) => {
                            debug!("mqtt: broker-initiated disconnect from '{}'", self.broker_url);
                        }
                        // Other packets (PingResp, PubAck, PubRec, PubRel, PubComp, ...) need no handling.
                        Ok(_) => {}
                        Err(e) => {
                            warn!("mqtt: connection error on '{}': {e}", self.broker_url);
                            if !outage_reported {
                                self.emit_unavailable_all(&tx).await;
                                outage_reported = true;
                            }
                            let sleep_fut = sleep(backoff);
                            tokio::select! {
                                () = cancel.cancelled() => {
                                    info!("mqtt source '{}' cancelled during backoff", self.broker_url);
                                    let _ = eventloop.disconnect().await;
                                    return Ok(());
                                }
                                () = sleep_fut => {}
                            }
                            backoff = backoff::next_backoff(backoff, BACKOFF_MIN, BACKOFF_MAX, JITTER_FRACTION);
                            eventloop.reconnect().await?;
                            pending_subacks.clear();
                            acknowledged_subscriptions = 0;
                            outgoing_subscriptions = 0;
                            queued_subscriptions = 0;
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl SensorSource for MqttSource {
    fn source_id(&self) -> &str {
        &self.broker_url
    }

    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<PresenceEvent>,
        ctl_tx: mpsc::Sender<ControlMsg>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        self.run_source(tx, ctl_tx, cancel).await
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
///    as trimmed bytes against those literals.  An empty payload never matches
///    (guards against accidental blank-payload triggers).
/// 2. Otherwise the payload is parsed as JSON, the configured `field` (JSON
///    pointer) is resolved, and the value is interpreted as a boolean.
/// 3. Non-bool string values `"ON"`, `"OFF"`, `"true"`, `"false"` (`Zigbee2MQTT`
///    variants) are also accepted.
#[must_use]
pub fn parse_payload(cfg: &MqttSensorCfg, payload: &[u8]) -> Option<SensorState> {
    // Literal payload match.
    if let Some(on) = &cfg.payload_on {
        let trimmed = String::from_utf8_lossy(payload).trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed == *on {
            return Some(SensorState::Present);
        }
    }
    if let Some(off) = &cfg.payload_off {
        let trimmed = String::from_utf8_lossy(payload).trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
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

/// Result of parsing an availability payload against a binding's configured
/// online/offline literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AvailabilityParse {
    /// Payload matched the configured "offline" literal.
    Offline,
    /// Payload matched the configured "online" literal.
    Online,
    /// Payload matched neither literal.
    Unrecognized,
}

/// Parse an availability payload against a binding's configured `online` /
/// `offline` literals.
///
/// Handles both plain-text (exact, case-sensitive match) and the JSON form
/// `{"state":"<literal>"}` (`Zigbee2MQTT` variant) for either literal.
/// Anything else is [`AvailabilityParse::Unrecognized`].
fn parse_availability(payload: &[u8], online: &str, offline: &str) -> AvailabilityParse {
    // Try plain text first.
    let text = String::from_utf8_lossy(payload).trim().to_string();
    if text == offline {
        return AvailabilityParse::Offline;
    }
    if text == online {
        return AvailabilityParse::Online;
    }

    // Try JSON.
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload)
        && let Some(state) = value.get("state").and_then(|v| v.as_str())
    {
        if state == offline {
            return AvailabilityParse::Offline;
        }
        if state == online {
            return AvailabilityParse::Online;
        }
    }

    AvailabilityParse::Unrecognized
}

/// Build the availability topic for a sensor topic.
///
/// `Zigbee2MQTT` convention: `<topic>/availability`.
#[must_use]
pub fn availability_topic(topic: &str) -> String {
    format!("{topic}/availability")
}

// ── Backoff helpers ───────────────────────────────────────────────────────────

// `next_backoff` lives in `crate::backoff` — shared with `ha_ws`.

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::schema::SensorKind;
    use rumqttc::ConnectReturnCode;
    use rumqttc::mqttbytes::v4::SubscribeReasonCode;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct ScriptedEventLoop {
        events: Arc<Mutex<VecDeque<Event>>>,
        subscriptions: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedEventLoop {
        fn new(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                events: Arc::new(Mutex::new(events.into_iter().collect())),
                subscriptions: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn subscribe_count(&self) -> usize {
            self.subscriptions.lock().expect("script lock").len()
        }
    }

    #[async_trait]
    impl MqttEventLoop for ScriptedEventLoop {
        async fn poll(&mut self) -> anyhow::Result<Event> {
            loop {
                if let Some(event) = self.events.lock().expect("script lock").pop_front() {
                    return Ok(event);
                }
                tokio::task::yield_now().await;
            }
        }

        async fn subscribe(&mut self, topic: &str, _qos: QoS) -> anyhow::Result<()> {
            self.subscriptions
                .lock()
                .expect("script lock")
                .push(topic.to_owned());
            Ok(())
        }

        async fn disconnect(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn reconnect(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

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
            availability_topic: None,
            availability_payload_online: "online".into(),
            availability_payload_offline: "offline".into(),
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
            availability_topic: None,
            availability_payload_online: "online".into(),
            availability_payload_offline: "offline".into(),
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
        assert!(matches!(
            parse_availability(payload, "online", "offline"),
            AvailabilityParse::Offline
        ));
    }

    #[test]
    fn availability_offline_maps_unavailable_json() {
        let payload = include_bytes!("../fixtures/z2m_availability.json");
        assert!(matches!(
            parse_availability(payload, "online", "offline"),
            AvailabilityParse::Offline
        ));
    }

    #[test]
    fn availability_online_returns_none() {
        assert!(matches!(
            parse_availability(b"online", "online", "offline"),
            AvailabilityParse::Online
        ));
        assert!(matches!(
            parse_availability(br#"{"state":"online"}"#, "online", "offline"),
            AvailabilityParse::Online
        ));
    }

    // ── parse_availability: tri-state + new behaviour (Task 2) ─────────────

    #[test]
    fn parse_availability_tri_state() {
        assert!(matches!(
            parse_availability(b"offline", "online", "offline"),
            AvailabilityParse::Offline
        ));
        assert!(matches!(
            parse_availability(b"online", "online", "offline"),
            AvailabilityParse::Online
        ));
        assert!(matches!(
            parse_availability(br#"{"state":"offline"}"#, "online", "offline"),
            AvailabilityParse::Offline
        ));
        // Case-sensitive, pinned.
        assert!(matches!(
            parse_availability(b"Offline", "online", "offline"),
            AvailabilityParse::Unrecognized
        ));
        // Custom literals.
        assert!(matches!(
            parse_availability(b"0", "1", "0"),
            AvailabilityParse::Offline
        ));
    }

    // ── parse_payload: literal match ───────────────────────────────────────

    #[test]
    fn literal_payload_match_for_non_json() {
        let cfg = make_cfg_literal("ON", "OFF");
        assert_eq!(parse_payload(&cfg, b"ON"), Some(SensorState::Present));
        assert_eq!(parse_payload(&cfg, b"OFF"), Some(SensorState::Absent));
        // Whitespace tolerance.
        assert_eq!(parse_payload(&cfg, b"  ON  "), Some(SensorState::Present));
        // Non-matching literal returns None.
        assert_eq!(parse_payload(&cfg, b"UNKNOWN"), None);
    }

    #[test]
    fn empty_payload_never_matches_literal() {
        let cfg = make_cfg_literal("ON", "OFF");
        assert_eq!(parse_payload(&cfg, b""), None);
        assert_eq!(parse_payload(&cfg, b"  "), None);
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
            availability_topic: None,
            availability_payload_online: "online".into(),
            availability_payload_offline: "offline".into(),
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

    // Backoff tests live in `crate::backoff::tests` — shared with `ha_ws`.

    // ── dispatch_publish ───────────────────────────────────────────────────

    #[test]
    fn shared_topic_fans_out_to_all_sensors() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![
                (
                    SensorId("desk_occupancy".into()),
                    MqttSensorCfg {
                        broker_url: "tcp://localhost:1883".into(),
                        topic: "sensors/desk".into(),
                        field: "/occupancy".into(),
                        payload_on: None,
                        payload_off: None,
                        kind: SensorKind::Presence,
                        hold_time: None,
                        stale_timeout: None,
                        availability_topic: None,
                        availability_payload_online: "online".into(),
                        availability_payload_offline: "offline".into(),
                    },
                ),
                (
                    SensorId("desk_presence".into()),
                    MqttSensorCfg {
                        broker_url: "tcp://localhost:1883".into(),
                        topic: "sensors/desk".into(),
                        field: "/presence".into(),
                        payload_on: None,
                        payload_off: None,
                        kind: SensorKind::Presence,
                        hold_time: None,
                        stale_timeout: None,
                        availability_topic: None,
                        availability_payload_online: "online".into(),
                        availability_payload_offline: "offline".into(),
                    },
                ),
            ],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        // Payload with both /occupancy and /presence fields.
        let payload = br#"{"occupancy":true,"presence":false}"#;
        let (events, _availability) = source.dispatch_publish(
            "sensors/desk",
            payload,
            &mut warned,
            &mut warned_availability,
        );

        assert_eq!(events.len(), 2, "both sensors should receive events");
        let by_id: HashMap<&str, SensorState> = events
            .iter()
            .map(|e| (e.sensor_id.0.as_str(), e.state))
            .collect();
        assert_eq!(by_id.get("desk_occupancy"), Some(&SensorState::Present));
        assert_eq!(by_id.get("desk_presence"), Some(&SensorState::Absent));
    }

    #[test]
    fn availability_topic_fans_out_to_all_sensors() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![
                (
                    SensorId("sensor_a".into()),
                    MqttSensorCfg {
                        broker_url: "tcp://localhost:1883".into(),
                        topic: "sensors/desk".into(),
                        field: "/occupancy".into(),
                        payload_on: None,
                        payload_off: None,
                        kind: SensorKind::Presence,
                        hold_time: None,
                        stale_timeout: None,
                        availability_topic: None,
                        availability_payload_online: "online".into(),
                        availability_payload_offline: "offline".into(),
                    },
                ),
                (
                    SensorId("sensor_b".into()),
                    MqttSensorCfg {
                        broker_url: "tcp://localhost:1883".into(),
                        topic: "sensors/desk".into(),
                        field: "/occupancy".into(),
                        payload_on: None,
                        payload_off: None,
                        kind: SensorKind::Presence,
                        hold_time: None,
                        stale_timeout: None,
                        availability_topic: None,
                        availability_payload_online: "online".into(),
                        availability_payload_offline: "offline".into(),
                    },
                ),
            ],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        let (events, _availability) = source.dispatch_publish(
            "sensors/desk/availability",
            b"offline",
            &mut warned,
            &mut warned_availability,
        );

        assert_eq!(events.len(), 2, "both sensors should get Unavailable");
        for event in &events {
            assert_eq!(event.state, SensorState::Unavailable);
        }
    }

    /// Issue #136: an `online` availability payload must surface as a
    /// [`SensorAvailabilityEvent`] with `online = true` so the rules
    /// engine's stale sweep leaves the sensor alone on topic silence.
    /// Deliberately produces NO presence event — an "online" frame is a
    /// reachability claim, not a state publish; emitting Present would
    /// defeat absence detection.
    #[test]
    fn online_payload_emits_availability_event_only() {
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
                    availability_topic: None,
                    availability_payload_online: "online".into(),
                    availability_payload_offline: "offline".into(),
                },
            )],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        let (events, availability) = source.dispatch_publish(
            "sensors/desk/availability",
            b"online",
            &mut warned,
            &mut warned_availability,
        );

        assert!(
            events.is_empty(),
            "online payload must NOT produce a presence event (got {events:?})"
        );
        assert_eq!(availability.len(), 1, "one availability event per binding");
        assert_eq!(availability[0].sensor, SensorId("desk".into()));
        assert!(
            availability[0].online,
            "online payload must surface as online = true (got {:?})",
            availability[0].online
        );
    }

    /// Issue #136: an `offline` availability payload (LWT) must BOTH
    /// demote the sensor to Unavailable AND emit a `SensorAvailabilityEvent`
    /// with `online = false` so the rules engine clears the reachability
    /// assertion.  This is the existing pre-#136 behavior plus the new
    /// availability edge.
    #[test]
    fn offline_payload_emits_unavailable_presence_and_offline_availability() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(
                SensorId("lwt".into()),
                MqttSensorCfg {
                    broker_url: "tcp://localhost:1883".into(),
                    topic: "sensors/lwt".into(),
                    field: "/occupancy".into(),
                    payload_on: None,
                    payload_off: None,
                    kind: SensorKind::Presence,
                    hold_time: None,
                    stale_timeout: None,
                    availability_topic: Some("sensors/lwt/LWT".into()),
                    availability_payload_online: "Online".into(),
                    availability_payload_offline: "Offline".into(),
                },
            )],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        let (events, availability) = source.dispatch_publish(
            "sensors/lwt/LWT",
            b"Offline",
            &mut warned,
            &mut warned_availability,
        );

        assert_eq!(events.len(), 1, "LWT offline emits one presence event");
        assert_eq!(events[0].state, SensorState::Unavailable);
        assert_eq!(events[0].sensor_id, SensorId("lwt".into()));

        assert_eq!(
            availability.len(),
            1,
            "LWT offline emits one availability event"
        );
        assert_eq!(availability[0].sensor, SensorId("lwt".into()));
        assert!(
            !availability[0].online,
            "LWT offline must surface as online = false (got {:?})",
            availability[0].online
        );
    }

    /// Pin: the existing no-availability-topic timeout behavior is
    /// preserved — `dispatch_publish` on a plain sensor topic never
    /// produces an availability event, regardless of the binding's
    /// configured online/offline literals.
    #[test]
    fn plain_sensor_topic_does_not_emit_availability_event() {
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
                    availability_topic: None,
                    availability_payload_online: "online".into(),
                    availability_payload_offline: "offline".into(),
                },
            )],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        let (events, availability) = source.dispatch_publish(
            "sensors/desk",
            br#"{"occupancy":true}"#,
            &mut warned,
            &mut warned_availability,
        );

        assert_eq!(events.len(), 1, "presence publish parses to one event");
        assert!(
            availability.is_empty(),
            "plain sensor topic must NOT produce availability events (got {availability:?})"
        );
    }

    #[test]
    fn dispatch_publish_warn_once_dedup() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("test".into()), make_cfg("/occupancy"))],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        // First bad payload → warn.
        let (events, _availability) = source.dispatch_publish(
            "test/sensor",
            b"garbage",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());
        assert!(warned.contains("test/sensor"));

        // Second bad payload → no warn (already warned).
        let (events, _availability) = source.dispatch_publish(
            "test/sensor",
            b"more garbage",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn dispatch_publish_unknown_topic_returns_empty() {
        let source = MqttSource::new("tcp://localhost:1883".into(), vec![], None);
        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        let (events, _availability) = source.dispatch_publish(
            "unknown/topic",
            b"data",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());
    }

    // ── dispatch_publish: availability routing + warn-once (Task 2) ────────

    #[test]
    fn override_topic_routes_to_availability_branch() {
        // spec F1: an `availability_topic` override must route through the
        // availability branch, not the sensor-topic parse path.
        let cfg = MqttSensorCfg {
            broker_url: "tcp://localhost:1883".into(),
            topic: "zigbee2mqtt/desk".into(),
            field: "/occupancy".into(),
            payload_on: None,
            payload_off: None,
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
            availability_topic: Some("tele/desk/LWT".into()),
            availability_payload_online: "online".into(),
            availability_payload_offline: "offline".into(),
        };
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("desk".into()), cfg)],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();
        let (events, _availability) = source.dispatch_publish(
            "tele/desk/LWT",
            b"offline",
            &mut warned,
            &mut warned_availability,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sensor_id, SensorId("desk".into()));
        assert_eq!(events[0].state, SensorState::Unavailable);
    }

    #[test]
    fn default_derivation_unchanged_without_override() {
        // Byte-identical behaviour pin: no override key -> topic_map and
        // availability_topics both contain "<topic>/availability".
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
            None,
        );
        assert!(source.topic_map.contains_key("test/sensor/availability"));
        assert!(
            source
                .availability_topics
                .contains("test/sensor/availability")
        );
    }

    #[test]
    fn warn_once_is_per_binding_not_per_topic() {
        // spec F8: two bindings sharing a topic, each with its own
        // online/offline literals, must warn independently.
        let cfg_a = MqttSensorCfg {
            broker_url: "tcp://localhost:1883".into(),
            topic: "shared/topic".into(),
            field: "/occupancy".into(),
            payload_on: None,
            payload_off: None,
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
            availability_topic: Some("shared/availability".into()),
            availability_payload_online: "online".into(),
            availability_payload_offline: "offline".into(),
        };
        let cfg_b = MqttSensorCfg {
            broker_url: "tcp://localhost:1883".into(),
            topic: "shared/topic2".into(),
            field: "/occupancy".into(),
            payload_on: None,
            payload_off: None,
            kind: SensorKind::Presence,
            hold_time: None,
            stale_timeout: None,
            availability_topic: Some("shared/availability".into()),
            availability_payload_online: "1".into(),
            availability_payload_offline: "0".into(),
        };
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("a".into()), cfg_a), (SensorId("b".into()), cfg_b)],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();

        // "1": A (online="online"/offline="offline") can't parse it ->
        // unrecognized -> A warns. B (online="1"/offline="0") parses Online
        // -> no event.
        let (events, _availability) = source.dispatch_publish(
            "shared/availability",
            b"1",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());
        assert!(
            warned_availability.contains(&("shared/availability".to_string(), "a".to_string()))
        );
        assert!(
            !warned_availability.contains(&("shared/availability".to_string(), "b".to_string()))
        );

        // "weird": both unrecognized -> B gets its own first warn.
        let (events, _availability) = source.dispatch_publish(
            "shared/availability",
            b"weird",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());
        assert!(
            warned_availability.contains(&("shared/availability".to_string(), "a".to_string()))
        );
        assert!(
            warned_availability.contains(&("shared/availability".to_string(), "b".to_string()))
        );
    }

    #[test]
    fn good_payload_still_parses_after_warns() {
        // Anti-tautology: after two unrecognized payloads, a well-formed
        // payload still emits Unavailable — the warn-once bookkeeping must
        // never suppress real parsing.
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
            None,
        );

        let mut warned = HashSet::new();
        let mut warned_availability = HashSet::new();

        let (events, _availability) = source.dispatch_publish(
            "test/sensor/availability",
            b"garbled1",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());
        let (events, _availability) = source.dispatch_publish(
            "test/sensor/availability",
            b"garbled2",
            &mut warned,
            &mut warned_availability,
        );
        assert!(events.is_empty());

        let (events, _availability) = source.dispatch_publish(
            "test/sensor/availability",
            b"offline",
            &mut warned,
            &mut warned_availability,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, SensorState::Unavailable);
    }

    // ── MqttSource construction ────────────────────────────────────────────

    #[test]
    fn source_id_returns_broker_url() {
        let source = MqttSource::new(
            "tcp://mqtt.local:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
            None,
        );
        assert_eq!(source.source_id(), "tcp://mqtt.local:1883");
    }

    #[test]
    fn topic_map_includes_sensor_and_availability() {
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
                    availability_topic: None,
                    availability_payload_online: "online".into(),
                    availability_payload_offline: "offline".into(),
                },
            )],
            None,
        );
        assert!(source.topic_map.contains_key("sensors/desk"));
        assert!(source.topic_map.contains_key("sensors/desk/availability"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emit_unavailable_all_deduplicates() {
        // Two sensors on the same topic — emit_unavailable_all should produce
        // exactly 2 events (not 4, since each sensor appears in both the
        // sensor topic and availability topic entries).
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![
                (SensorId("a".into()), make_cfg("/occupancy")),
                (SensorId("b".into()), make_cfg("/occupancy")),
            ],
            None,
        );

        let (tx, mut rx) = mpsc::channel(8);
        source.emit_unavailable_all(&tx).await;
        drop(tx);

        let mut events: Vec<PresenceEvent> = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert_eq!(events.len(), 2, "exactly 2 events, one per sensor");
        let mut ids: HashSet<&str> = HashSet::new();
        for event in &events {
            assert_eq!(event.state, SensorState::Unavailable);
            ids.insert(event.sensor_id.0.as_str());
        }
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
    }

    // ── Credential wiring ──────────────────────────────────────────────────

    #[test]
    fn new_stores_credential_when_present() {
        let cred = MqttCredential {
            username: "alice".into(),
            password: "s3cret".into(),
        };
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("s".into()), make_cfg("/occupancy"))],
            Some(cred.clone()),
        );
        let wired = source.credential().expect("credential should be present");
        assert_eq!(wired.username, "alice");
        assert_eq!(wired.password, "s3cret");
    }

    #[test]
    fn new_stores_none_when_no_credential() {
        let source = MqttSource::new("tcp://localhost:1883".into(), vec![], None);
        assert!(source.credential().is_none());
    }

    #[test]
    fn subscription_ack_succeeds_only_when_all_codes_success() {
        assert!(MqttSource::subscription_ack_succeeded(&[
            SubscribeReasonCode::Success(QoS::AtMostOnce),
            SubscribeReasonCode::Success(QoS::AtLeastOnce),
        ]));
        assert!(!MqttSource::subscription_ack_succeeded(&[
            SubscribeReasonCode::Success(QoS::AtMostOnce),
            SubscribeReasonCode::Failure,
        ]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_connack_queues_one_batch_and_no_batch_before_connack() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
            None,
        );
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
        let fake = ScriptedEventLoop::new([
            Event::Incoming(Packet::ConnAck(rumqttc::ConnAck {
                session_present: false,
                code: ConnectReturnCode::Success,
            })),
            Event::Outgoing(Outgoing::Subscribe(1)),
            Event::Outgoing(Outgoing::Subscribe(2)),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 1,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 2,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
        ]);
        let (tx, _rx) = mpsc::channel(4);
        let (ctl_tx, _ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let run = tokio::spawn(
            Box::new(source.with_lifecycle_sender(lifecycle_tx)).run_with_event_loop(
                Box::new(fake.clone()),
                tx,
                ctl_tx,
                cancel.clone(),
            ),
        );
        let mut lifecycle = Vec::new();
        while lifecycle.len() < 3 {
            lifecycle.push(
                tokio::time::timeout(Duration::from_secs(1), lifecycle_rx.recv())
                    .await
                    .expect("lifecycle event timeout")
                    .expect("lifecycle event"),
            );
        }
        assert_eq!(fake.subscribe_count(), 2);
        assert_eq!(lifecycle[0], MqttLifecycle::Connected);
        assert_eq!(lifecycle[1], MqttLifecycle::SubscribeQueued { count: 2 });
        assert_eq!(lifecycle[2], MqttLifecycle::Subscribed);
        cancel.cancel();
        run.await.expect("source task").expect("source run");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_reconnack_resubscribes_and_resets_ack_counters() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
            None,
        );
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
        let fake = ScriptedEventLoop::new([
            Event::Incoming(Packet::ConnAck(rumqttc::ConnAck {
                session_present: false,
                code: ConnectReturnCode::Success,
            })),
            Event::Outgoing(Outgoing::Subscribe(1)),
            Event::Outgoing(Outgoing::Subscribe(2)),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 1,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 2,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
            Event::Incoming(Packet::ConnAck(rumqttc::ConnAck {
                session_present: false,
                code: ConnectReturnCode::Success,
            })),
            Event::Outgoing(Outgoing::Subscribe(3)),
            Event::Outgoing(Outgoing::Subscribe(4)),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 3,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 4,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
        ]);
        let (tx, _rx) = mpsc::channel(4);
        let (ctl_tx, _ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let run = tokio::spawn(
            Box::new(source.with_lifecycle_sender(lifecycle_tx)).run_with_event_loop(
                Box::new(fake.clone()),
                tx,
                ctl_tx,
                cancel.clone(),
            ),
        );
        let mut subscribed = 0;
        while subscribed < 2 {
            if tokio::time::timeout(Duration::from_secs(1), lifecycle_rx.recv())
                .await
                .expect("lifecycle event timeout")
                == Some(MqttLifecycle::Subscribed)
            {
                subscribed += 1;
            }
        }
        assert_eq!(fake.subscribe_count(), 4);
        cancel.cancel();
        run.await.expect("source task").expect("source run");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_rejected_suback_does_not_emit_subscribed() {
        let source = MqttSource::new(
            "tcp://localhost:1883".into(),
            vec![(SensorId("desk".into()), make_cfg("/occupancy"))],
            None,
        );
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
        let fake = ScriptedEventLoop::new([
            Event::Incoming(Packet::ConnAck(rumqttc::ConnAck {
                session_present: false,
                code: ConnectReturnCode::Success,
            })),
            Event::Outgoing(Outgoing::Subscribe(1)),
            Event::Outgoing(Outgoing::Subscribe(2)),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 1,
                return_codes: vec![SubscribeReasonCode::Failure],
            })),
            Event::Incoming(Packet::SubAck(rumqttc::SubAck {
                pkid: 2,
                return_codes: vec![SubscribeReasonCode::Success(QoS::AtLeastOnce)],
            })),
        ]);
        let (tx, _rx) = mpsc::channel(4);
        let (ctl_tx, _ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let run = tokio::spawn(
            Box::new(source.with_lifecycle_sender(lifecycle_tx)).run_with_event_loop(
                Box::new(fake),
                tx,
                ctl_tx,
                cancel.clone(),
            ),
        );
        assert_eq!(lifecycle_rx.recv().await, Some(MqttLifecycle::Connected));
        assert_eq!(
            lifecycle_rx.recv().await,
            Some(MqttLifecycle::SubscribeQueued { count: 2 })
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), lifecycle_rx.recv())
                .await
                .is_err()
        );
        cancel.cancel();
        run.await.expect("source task").expect("source run");
    }
}
