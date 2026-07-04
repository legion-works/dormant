//! Home Assistant WebSocket sensor source — subscribes to entity state via the
//! HA `subscribe_entities` API and maps state changes to [`PresenceEvent`]s.
//!
//! ## Architecture
//!
//! One [`HaWsSource`] per distinct HA `url` — it multiplexes all sensors sharing
//! that URL into a single WebSocket connection.  The protocol state machine
//! (`HaProtocol`) is pure and fully testable without a socket.
//!
//! ## Fail-safe behaviour
//!
//! - Connection loss / auth failure → emit [`SensorState::Unavailable`] for all
//!   owned sensors once per outage, then reconnect with backoff.
//! - Auth failure gets a long backoff (60s) since the token may need manual
//!   replacement.
//! - On reconnect the protocol restarts from scratch (fresh auth handshake).

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::E_HA_AUTH;
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff;

// ── Constants ──────────────────────────────────────────────────────────────────

/// Minimum reconnect backoff for transient errors.
const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Maximum reconnect backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Backoff for auth failures (token may need manual replacement).
const AUTH_BACKOFF: Duration = Duration::from_secs(60);

/// Jitter fraction (±20%).
const JITTER_FRACTION: f64 = 0.20;

// ── HaWsSource ─────────────────────────────────────────────────────────────────

/// A Home Assistant WebSocket sensor source that multiplexes multiple sensors
/// on one HA instance.
pub struct HaWsSource {
    /// HA WebSocket URL (e.g. `ws://ha.local:8123/api/websocket`).
    url: String,
    /// Long-lived access token.
    token: String,
    /// Per-sensor entity IDs, paired with their stable [`SensorId`].
    entities: Vec<(SensorId, String)>,
}

impl HaWsSource {
    /// Create a new `HaWsSource`.
    #[must_use]
    pub fn new(url: String, token: String, entities: Vec<(SensorId, String)>) -> Self {
        Self {
            url,
            token,
            entities,
        }
    }

    /// Emit [`SensorState::Unavailable`] for every owned sensor.
    async fn emit_unavailable_all(&self, tx: &mpsc::Sender<PresenceEvent>) {
        let ids: Vec<SensorId> = self.entities.iter().map(|(id, _)| id.clone()).collect();
        backoff::emit_unavailable_all(&ids, tx).await;
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl SensorSource for HaWsSource {
    fn source_id(&self) -> &str {
        &self.url
    }

    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<PresenceEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut backoff = BACKOFF_MIN;
        let mut outage_reported = false;

        loop {
            // Attempt connection.
            let ws_result = tokio::select! {
                () = cancel.cancelled() => {
                    info!("ha-ws source '{}' cancelled", self.url);
                    return Ok(());
                }
                result = connect_async(&self.url) => result,
            };

            let (ws_stream, _) = match ws_result {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("ha-ws: connection failed to '{}': {e}", self.url);
                    if !outage_reported {
                        self.emit_unavailable_all(&tx).await;
                        outage_reported = true;
                    }
                    sleep(backoff).await;
                    backoff =
                        backoff::next_backoff(backoff, BACKOFF_MIN, BACKOFF_MAX, JITTER_FRACTION);
                    continue;
                }
            };

            let (mut write, read) = ws_stream.split();
            let mut protocol = HaProtocol::new(self.token.clone(), self.entities.clone());

            // ── Inner read loop ────────────────────────────────────────────
            let mut read = read;
            let mut auth_backoff = false;
            let mut write_error = false;

            'inner: loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        info!("ha-ws source '{}' cancelled", self.url);
                        return Ok(());
                    }
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                let actions = protocol.handle_message(&text);
                                for action in actions {
                                    match action {
                                        Action::SendText(out) => {
                                            debug!("ha-ws: sending: {out}");
                                            if write
                                                .send(Message::Text(out))
                                                .await
                                                .is_err()
                                            {
                                                write_error = true;
                                                break 'inner;
                                            }
                                        }
                                        Action::Emit(event) => {
                                            if tx.send(event).await.is_err() {
                                                return Ok(());
                                            }
                                        }
                                        Action::Fatal(reason) => {
                                            warn!("ha-ws: fatal on '{}': {reason}", self.url);
                                            self.emit_unavailable_all(&tx).await;
                                            auth_backoff = true;
                                            break 'inner;
                                        }
                                        Action::Nothing => {}
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => {
                                // Tungstenite handles ping/pong automatically.
                                // Binary frames and raw frames are unexpected — ignore.
                            }
                            Some(Ok(Message::Close(_))) => {
                                debug!("ha-ws: server closed connection on '{}'", self.url);
                                if !outage_reported {
                                    self.emit_unavailable_all(&tx).await;
                                    outage_reported = true;
                                }
                                break 'inner;
                            }
                            Some(Err(e)) => {
                                warn!("ha-ws: read error on '{}': {e}", self.url);
                                if !outage_reported {
                                    self.emit_unavailable_all(&tx).await;
                                    outage_reported = true;
                                }
                                break 'inner;
                            }
                            None => {
                                // Stream ended.
                                if !outage_reported {
                                    self.emit_unavailable_all(&tx).await;
                                    outage_reported = true;
                                }
                                break 'inner;
                            }
                        }
                    }
                }
            }

            if write_error && !outage_reported {
                self.emit_unavailable_all(&tx).await;
            }

            // Reconnect backoff.
            if auth_backoff {
                sleep(AUTH_BACKOFF).await;
            } else {
                sleep(backoff).await;
                backoff = backoff::next_backoff(backoff, BACKOFF_MIN, BACKOFF_MAX, JITTER_FRACTION);
            }
            outage_reported = false;
        }
    }
}

// ── Protocol state machine ─────────────────────────────────────────────────────

/// Phase of the HA WebSocket authentication handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Waiting for `auth_required` from the server.
    AwaitingAuthRequired,
    /// Sent auth token, waiting for `auth_ok` or `auth_invalid`.
    AuthSent,
    /// Authenticated, subscribed to entities, receiving events.
    Subscribed,
}

/// An action produced by the protocol state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send this text frame to the server.
    SendText(String),
    /// Emit this presence event to the daemon.
    Emit(PresenceEvent),
    /// Fatal error — log the message and reconnect.
    Fatal(String),
    /// No action needed.
    Nothing,
}

/// Pure protocol state machine for the HA WebSocket `subscribe_entities` flow.
///
/// Fully testable without a socket — feed it JSON strings via
/// [`handle_message`](Self::handle_message) and inspect the returned
/// [`Action`]s.
pub struct HaProtocol {
    /// Current protocol phase.
    phase: Phase,
    /// Next message ID to use for outgoing commands.
    next_id: u64,
    /// Long-lived access token.
    token: String,
    /// Entity → [`SensorId`] mapping.
    entities: Vec<(SensorId, String)>,
    /// Set of entity IDs we are subscribed to.
    subscribed_entities: Vec<String>,
    /// Entities we've warned about (unknown entities, one warning each).
    warned_entities: HashSet<String>,
    /// (`entity_id`, `state_string`) pairs we've warned about for unrecognised
    /// states — one warning per unique combination.
    warned_states: HashSet<(String, String)>,
}

impl HaProtocol {
    /// Create a new protocol state machine.
    #[must_use]
    pub fn new(token: String, entities: Vec<(SensorId, String)>) -> Self {
        let subscribed_entities: Vec<String> = entities.iter().map(|(_, e)| e.clone()).collect();
        Self {
            phase: Phase::AwaitingAuthRequired,
            next_id: 1,
            token,
            entities,
            subscribed_entities,
            warned_entities: HashSet::new(),
            warned_states: HashSet::new(),
        }
    }

    /// Process an incoming WebSocket text message and return the resulting
    /// actions.
    ///
    /// The caller must execute each action in order.
    pub fn handle_message(&mut self, raw: &str) -> Vec<Action> {
        // Parse the JSON envelope.
        let value: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("ha-ws: malformed JSON from server: {e}");
                return vec![Action::Nothing];
            }
        };

        // Extract the "type" field.
        let Some(msg_type) = value.get("type").and_then(|v| v.as_str()) else {
            warn!("ha-ws: message without 'type' field");
            return vec![Action::Nothing];
        };

        match self.phase {
            Phase::AwaitingAuthRequired => {
                if msg_type == "auth_required" {
                    self.phase = Phase::AuthSent;
                    let auth_msg = serde_json::json!({
                        "type": "auth",
                        "access_token": self.token,
                    });
                    vec![Action::SendText(auth_msg.to_string())]
                } else {
                    warn!(
                        "ha-ws: expected 'auth_required', got '{msg_type}' (phase=AwaitingAuthRequired)"
                    );
                    vec![Action::Fatal(format!(
                        "expected auth_required, got '{msg_type}'"
                    ))]
                }
            }
            Phase::AuthSent => match msg_type {
                "auth_ok" => {
                    self.phase = Phase::Subscribed;
                    let id = self.next_id;
                    self.next_id += 1;
                    let sub_msg = serde_json::json!({
                        "id": id,
                        "type": "subscribe_entities",
                        "entity_ids": self.subscribed_entities,
                    });
                    vec![Action::SendText(sub_msg.to_string())]
                }
                "auth_invalid" => {
                    let message = value
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown reason");
                    vec![Action::Fatal(format!("{E_HA_AUTH}: {message}"))]
                }
                other => {
                    warn!(
                        "ha-ws: expected 'auth_ok' or 'auth_invalid', got '{other}' (phase=AuthSent)"
                    );
                    vec![Action::Fatal(format!(
                        "unexpected message type '{other}' during auth"
                    ))]
                }
            },
            Phase::Subscribed => match msg_type {
                "event" => self.handle_event(&value),
                "result" => {
                    // Check for success/failure.
                    let success = value
                        .get("success")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if success {
                        vec![Action::Nothing]
                    } else {
                        let error_msg = value
                            .get("error")
                            .and_then(|v| v.get("message"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        vec![Action::Fatal(format!(
                            "subscribe_entities failed: {error_msg}"
                        ))]
                    }
                }
                other => {
                    debug!("ha-ws: unhandled message type '{other}' in Subscribed phase");
                    vec![Action::Nothing]
                }
            },
        }
    }

    /// Handle an `event` message in the Subscribed phase.
    ///
    /// The event payload contains `a` (add — full state) and `c` (change —
    /// partial state) maps.  Each key is an `entity_id`, and the value contains
    /// an `"s"` field with the state string.
    fn handle_event(&mut self, value: &serde_json::Value) -> Vec<Action> {
        let Some(event) = value.get("event") else {
            return vec![Action::Nothing];
        };

        let mut actions: Vec<Action> = Vec::new();

        // Process "a" (add — initial full state).
        if let Some(adds) = event.get("a").and_then(|v| v.as_object()) {
            for (entity_id, state_value) in adds {
                self.process_entity_state(entity_id, state_value, &mut actions);
            }
        }

        // Process "c" (change — partial state, nested under "+").
        if let Some(changes) = event.get("c").and_then(|v| v.as_object()) {
            for (entity_id, change_value) in changes {
                if let Some(plus) = change_value.get("+") {
                    self.process_entity_state(entity_id, plus, &mut actions);
                }
            }
        }

        actions
    }

    /// Map an entity state value to actions.
    ///
    /// Fans out to ALL sensor ids that share this `entity_id` (not just the
    /// first match), so that two sensors watching the same entity both
    /// receive the event.
    fn process_entity_state(
        &mut self,
        entity_id: &str,
        state_value: &serde_json::Value,
        actions: &mut Vec<Action>,
    ) {
        // Collect ALL sensor ids for this entity.
        let sensor_ids: Vec<SensorId> = self
            .entities
            .iter()
            .filter(|(_, e)| e == entity_id)
            .map(|(id, _)| id.clone())
            .collect();

        if sensor_ids.is_empty() {
            if self.warned_entities.insert(entity_id.to_string()) {
                warn!("ha-ws: received state for unknown entity '{entity_id}' (first occurrence)");
            } else {
                debug!("ha-ws: received state for unknown entity '{entity_id}' (suppressed)");
            }
            return;
        }

        // Extract the "s" (state) field.
        let Some(state_str) = state_value.get("s").and_then(|v| v.as_str()) else {
            debug!("ha-ws: entity '{entity_id}' state value has no 's' field");
            return;
        };

        let state = match state_str {
            "on" => SensorState::Present,
            "off" => SensorState::Absent,
            "unavailable" | "unknown" => SensorState::Unavailable,
            other => {
                let key = (entity_id.to_string(), other.to_string());
                if self.warned_states.insert(key) {
                    warn!(
                        "ha-ws: entity '{entity_id}' has unrecognised state '{other}' (first occurrence)"
                    );
                } else {
                    debug!(
                        "ha-ws: entity '{entity_id}' has unrecognised state '{other}' (suppressed)"
                    );
                }
                return;
            }
        };

        for sensor_id in sensor_ids {
            actions.push(Action::Emit(PresenceEvent::new(
                sensor_id,
                state,
                Timestamp::now(),
            )));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use dormant_core::types::SensorState;

    // ── Fixture helpers ────────────────────────────────────────────────────

    fn make_protocol() -> HaProtocol {
        HaProtocol::new(
            "test_token".into(),
            vec![(
                SensorId("motion".into()),
                "binary_sensor.test_motion".into(),
            )],
        )
    }

    // ── Auth flow ──────────────────────────────────────────────────────────

    #[test]
    fn auth_flow_happy_path() {
        let mut proto = make_protocol();

        // auth_required → SendText with auth token
        let actions = proto.handle_message(r#"{"type":"auth_required"}"#);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::SendText(text) => {
                let v: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(v["type"], "auth");
                assert_eq!(v["access_token"], "test_token");
            }
            other => panic!("expected SendText, got {other:?}"),
        }

        // auth_ok → SendText with subscribe_entities
        let actions = proto.handle_message(r#"{"type":"auth_ok"}"#);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::SendText(text) => {
                let v: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(v["type"], "subscribe_entities");
                assert_eq!(v["id"], 1);
                assert_eq!(
                    v["entity_ids"],
                    serde_json::json!(["binary_sensor.test_motion"])
                );
            }
            other => panic!("expected SendText, got {other:?}"),
        }

        // result success → Nothing
        let actions = proto.handle_message(r#"{"id":1,"type":"result","success":true}"#);
        assert_eq!(actions, vec![Action::Nothing]);
    }

    #[test]
    fn auth_flow_uses_fixture() {
        let mut proto = make_protocol();
        let fixture: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../fixtures/ha_auth_flow.json")).unwrap();

        // auth_required
        let actions = proto.handle_message(&fixture[0].to_string());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendText(_)));

        // auth_ok
        let actions = proto.handle_message(&fixture[1].to_string());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendText(_)));

        // result success
        let actions = proto.handle_message(&fixture[2].to_string());
        assert_eq!(actions, vec![Action::Nothing]);
    }

    #[test]
    fn auth_invalid_is_fatal() {
        let mut proto = make_protocol();

        // auth_required
        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);

        // auth_invalid → Fatal
        let actions =
            proto.handle_message(r#"{"type":"auth_invalid","message":"Invalid password"}"#);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Fatal(msg) => {
                assert!(
                    msg.contains(E_HA_AUTH),
                    "fatal message should contain E_HA_AUTH: {msg}"
                );
                assert!(msg.contains("Invalid password"));
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    // ── Event handling ─────────────────────────────────────────────────────

    #[test]
    fn add_event_maps_on_to_present() {
        let mut proto = make_protocol();

        // Advance to Subscribed phase.
        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // Send an "a" (add) event with "on" state.
        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"on"}}}}"#,
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Emit(event) => {
                assert_eq!(event.sensor_id, SensorId("motion".into()));
                assert_eq!(event.state, SensorState::Present);
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn change_event_maps_off_to_absent() {
        let mut proto = make_protocol();

        // Advance to Subscribed phase.
        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // Send a "c" (change) event with "off" state under "+".
        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"c":{"binary_sensor.test_motion":{"+":{"s":"off"}}}}}"#,
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Emit(event) => {
                assert_eq!(event.sensor_id, SensorId("motion".into()));
                assert_eq!(event.state, SensorState::Absent);
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn fixture_add_and_change_events() {
        let mut proto = make_protocol();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/ha_subscribe_entities_event.json"))
                .unwrap();

        // Advance to Subscribed phase.
        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // The fixture has both "a" (add, "on") and "c" (change, "off").
        let actions = proto.handle_message(&fixture.to_string());
        assert_eq!(actions.len(), 2, "should produce two Emit actions");

        // First: add event → Present
        match &actions[0] {
            Action::Emit(event) => {
                assert_eq!(event.sensor_id, SensorId("motion".into()));
                assert_eq!(event.state, SensorState::Present);
            }
            other => panic!("expected Emit, got {other:?}"),
        }

        // Second: change event → Absent
        match &actions[1] {
            Action::Emit(event) => {
                assert_eq!(event.sensor_id, SensorId("motion".into()));
                assert_eq!(event.state, SensorState::Absent);
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn unavailable_state_maps_unavailable() {
        let mut proto = make_protocol();

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"unavailable"}}}}"#,
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Emit(event) => assert_eq!(event.state, SensorState::Unavailable),
            other => panic!("expected Emit, got {other:?}"),
        }

        // Also test "unknown"
        let actions = proto.handle_message(
            r#"{"id":2,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"unknown"}}}}"#,
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Emit(event) => assert_eq!(event.state, SensorState::Unavailable),
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn unknown_entity_in_event_ignored_warn_once() {
        let mut proto = make_protocol();

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // Unknown entity should be ignored (no actions).
        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.unknown":{"s":"on"}}}}"#,
        );
        assert!(actions.is_empty(), "expected no actions for unknown entity");

        // Second occurrence of same unknown entity should also be ignored.
        let actions = proto.handle_message(
            r#"{"id":2,"type":"event","event":{"a":{"binary_sensor.unknown":{"s":"off"}}}}"#,
        );
        assert!(
            actions.is_empty(),
            "expected no actions for unknown entity (2nd)"
        );
    }

    #[test]
    fn result_failure_fatal() {
        let mut proto = make_protocol();

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        let actions = proto.handle_message(
            r#"{"id":1,"type":"result","success":false,"error":{"code":"not_found","message":"entity not found"}}"#,
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Fatal(msg) => assert!(msg.contains("entity not found")),
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[test]
    fn state_mapping_unknown_string_warns_nothing() {
        let mut proto = make_protocol();

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // An unrecognised state string should produce no actions (warn logged).
        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"bogus"}}}}"#,
        );
        assert!(actions.is_empty(), "expected no actions for unknown state");
    }

    #[test]
    fn change_event_without_s_key_ignored() {
        let mut proto = make_protocol();

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // A "c" change with "+" object that has no "s" field → ignored.
        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"c":{"binary_sensor.test_motion":{"+":{"t":"123"}}}}}"#,
        );
        assert!(
            actions.is_empty(),
            "expected no actions when 's' field is missing"
        );
    }

    #[test]
    fn two_sensors_same_entity_both_receive() {
        let mut proto = HaProtocol::new(
            "tok".into(),
            vec![
                (SensorId("motion_a".into()), "binary_sensor.shared".into()),
                (SensorId("motion_b".into()), "binary_sensor.shared".into()),
            ],
        );

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.shared":{"s":"on"}}}}"#,
        );
        assert_eq!(actions.len(), 2, "both sensors should receive the event");

        let mut sensor_ids: Vec<&str> = actions
            .iter()
            .map(|a| match a {
                Action::Emit(e) => e.sensor_id.0.as_str(),
                _ => panic!("expected Emit"),
            })
            .collect();
        sensor_ids.sort_unstable();
        assert_eq!(sensor_ids, vec!["motion_a", "motion_b"]);
    }

    #[test]
    fn unknown_state_dedup_warns_once() {
        let mut proto = make_protocol();

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        // First occurrence of "bogus" state → no actions.
        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"bogus"}}}}"#,
        );
        assert!(actions.is_empty(), "expected no actions for unknown state");

        // Second occurrence of same "bogus" state → also no actions (dedup).
        let actions = proto.handle_message(
            r#"{"id":2,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"bogus"}}}}"#,
        );
        assert!(
            actions.is_empty(),
            "expected no actions for repeated unknown state"
        );
    }

    // ── Edge cases ─────────────────────────────────────────────────────────

    #[test]
    fn malformed_json_returns_nothing() {
        let mut proto = make_protocol();
        let actions = proto.handle_message("not json at all");
        assert_eq!(actions, vec![Action::Nothing]);
    }

    #[test]
    fn message_without_type_returns_nothing() {
        let mut proto = make_protocol();
        let actions = proto.handle_message(r#"{"id":1}"#);
        assert_eq!(actions, vec![Action::Nothing]);
    }

    #[test]
    fn unexpected_message_during_auth_is_fatal() {
        let mut proto = make_protocol();
        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);

        // Unexpected message type during AuthSent phase.
        let actions = proto.handle_message(r#"{"type":"pong"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::Fatal(_)));
    }

    #[test]
    fn unexpected_message_during_awaiting_auth_is_fatal() {
        let mut proto = make_protocol();
        // Server sends something other than auth_required first.
        let actions = proto.handle_message(r#"{"type":"auth_ok"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::Fatal(_)));
    }

    #[test]
    fn multiple_entities_all_mapped() {
        let mut proto = HaProtocol::new(
            "tok".into(),
            vec![
                (SensorId("motion".into()), "binary_sensor.motion".into()),
                (SensorId("door".into()), "binary_sensor.door".into()),
            ],
        );

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);
        let _ = proto.handle_message(r#"{"type":"auth_ok"}"#);

        let actions = proto.handle_message(
            r#"{"id":1,"type":"event","event":{"a":{"binary_sensor.motion":{"s":"on"},"binary_sensor.door":{"s":"off"}}}}"#,
        );
        assert_eq!(actions.len(), 2, "should produce two Emit actions");

        // Collect sensor_id → state mapping (order is non-deterministic from JSON).
        let mut by_sensor: std::collections::BTreeMap<&str, SensorState> =
            std::collections::BTreeMap::new();
        for action in &actions {
            if let Action::Emit(event) = action {
                by_sensor.insert(&event.sensor_id.0, event.state);
            } else {
                panic!("expected Emit, got {action:?}");
            }
        }

        assert_eq!(by_sensor.get("motion"), Some(&SensorState::Present));
        assert_eq!(by_sensor.get("door"), Some(&SensorState::Absent));
    }

    // ── HaWsSource construction ────────────────────────────────────────────

    #[test]
    fn source_id_returns_url() {
        let source = HaWsSource::new(
            "ws://ha.local:8123/api/websocket".into(),
            "token".into(),
            vec![(SensorId("motion".into()), "binary_sensor.motion".into())],
        );
        assert_eq!(source.source_id(), "ws://ha.local:8123/api/websocket");
    }
}
