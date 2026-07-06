//! Home Assistant WebSocket probe — authenticates and reads entity state.

use crate::types::ProbeResult;
use dormant_core::config::schema::{Credentials, HaSensorCfg};
use dormant_core::error::E_HA_AUTH;
use dormant_core::types::{SensorId, SensorState};
use dormant_sensors::ha_ws::{Action, HaProtocol};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Probe all HA-configured sensors.
pub async fn probe_ha_all(
    cfg: &dormant_core::config::Config,
    creds: &Credentials,
) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for (id, sensor_cfg) in &cfg.sensors {
        if let dormant_core::config::schema::SensorConfig::Ha(ha_cfg) = sensor_cfg {
            results.push(probe_ha_one(id, ha_cfg, creds).await);
        }
    }
    if results.is_empty() {
        results.push(ProbeResult::skip("ha", "no HA sensors configured"));
    }
    results
}

/// Probe a single HA sensor by connecting WebSocket and subscribing.
pub(crate) async fn probe_ha_one(id: &str, cfg: &HaSensorCfg, creds: &Credentials) -> ProbeResult {
    let name = format!("ha {id}");

    let token = match &creds.ha_token {
        Some(t) => t.clone(),
        None => {
            return ProbeResult::fail(name, "no ha_token configured in credentials".to_string());
        }
    };

    // Connect WebSocket.
    let ws_result = tokio_tungstenite::connect_async(&cfg.url).await;
    let (ws_stream, _) = match ws_result {
        Ok(pair) => pair,
        Err(e) => {
            return ProbeResult::fail(name, format!("WebSocket connection failed: {e}"));
        }
    };

    let (mut write, read) = ws_stream.split();
    let mut protocol = HaProtocol::new(token, vec![(SensorId(id.into()), cfg.entity.clone())]);

    let mut read = read;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found_state: Option<SensorState> = None;
    let mut auth_failure: Option<String> = None;

    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        let msg = tokio::time::timeout(timeout, read.next()).await;

        match msg {
            Ok(Some(Ok(Message::Text(text)))) => {
                let actions = protocol.handle_message(&text);
                for action in actions {
                    match action {
                        Action::SendText(out) => {
                            if write.send(Message::Text(out)).await.is_err() {
                                return ProbeResult::fail(
                                    name,
                                    "WebSocket write failed".to_string(),
                                );
                            }
                        }
                        Action::Emit(event) => {
                            found_state = Some(event.state);
                        }
                        Action::Fatal(reason) => {
                            auth_failure = Some(reason);
                        }
                        Action::Nothing => {}
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_))) | None) => break,
            Ok(Some(Ok(_))) => {} // ping/pong/binary — ignore
            Ok(Some(Err(e))) => {
                return ProbeResult::fail(name, format!("WebSocket error: {e}"));
            }
            Err(_elapsed) => break, // timeout
        }

        if found_state.is_some() || auth_failure.is_some() {
            break;
        }
    }

    if let Some(state) = found_state {
        let state_str = match state {
            SensorState::Present => "present",
            SensorState::Absent => "absent",
            SensorState::Unavailable => "unavailable",
        };
        ProbeResult::pass(name, format!("entity '{}' reports {state_str}", cfg.entity))
    } else if let Some(reason) = auth_failure {
        if reason.contains(E_HA_AUTH) {
            ProbeResult::fail(name, format!("{reason} (check ha_token)"))
        } else {
            ProbeResult::fail(name, reason)
        }
    } else {
        ProbeResult::fail(
            name,
            format!("no state received from entity '{}' within 10s", cfg.entity),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the HA probe's message-handling path works with canned
    /// messages (same pattern as `HaProtocol` tests).
    #[test]
    fn ha_probe_protocol_auth_flow() {
        let mut proto = HaProtocol::new(
            "test_token".into(),
            vec![(
                SensorId("motion".into()),
                "binary_sensor.test_motion".into(),
            )],
        );

        // auth_required → SendText
        let actions = proto.handle_message(r#"{"type":"auth_required"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendText(_)));

        // auth_ok → SendText (subscribe)
        let actions = proto.handle_message(r#"{"type":"auth_ok"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::SendText(_)));

        // result success → Nothing
        let actions = proto.handle_message(r#"{"id":1,"type":"result","success":true}"#);
        assert_eq!(actions, vec![Action::Nothing]);

        // event with "on" state → Emit Present
        let actions = proto.handle_message(
            r#"{"id":2,"type":"event","event":{"a":{"binary_sensor.test_motion":{"s":"on"}}}}"#,
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
    fn ha_probe_protocol_auth_failure() {
        let mut proto = HaProtocol::new(
            "bad_token".into(),
            vec![(
                SensorId("motion".into()),
                "binary_sensor.test_motion".into(),
            )],
        );

        let _ = proto.handle_message(r#"{"type":"auth_required"}"#);

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
}
