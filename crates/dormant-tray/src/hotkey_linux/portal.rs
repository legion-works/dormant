//! XDG `GlobalShortcuts` portal fallback.

use std::collections::HashMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::{accelerator_tokens, claim_action};
use crate::hotkey::{Accelerator, HotkeyError, HotkeyRegistrar};
use crate::menu::Action;

const SERVICE: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

pub(super) struct PortalHotkeyRegistrar {
    conn: Option<zbus::Connection>,
    session: Option<OwnedObjectPath>,
    signal_task: Option<tokio::task::JoinHandle<()>>,
}

impl PortalHotkeyRegistrar {
    pub(super) const fn new() -> Self {
        Self {
            conn: None,
            session: None,
            signal_task: None,
        }
    }
}

fn accelerator_string(accelerator: &Accelerator) -> Result<String, HotkeyError> {
    let (modifiers, key) = accelerator_tokens(accelerator)?;
    let mut converted = Vec::with_capacity(modifiers.len() + 1);
    for modifier in modifiers {
        converted.push(match modifier {
            "Control" => "CTRL",
            "Alt" => "ALT",
            "Shift" => "SHIFT",
            "Meta" | "Super" => "LOGO",
            _ => return Err(HotkeyError::InvalidAccelerator(accelerator.raw.clone())),
        });
    }
    let key = match key {
        "Space" => "space".to_string(),
        "Enter" => "Return".to_string(),
        "Backspace" => "BackSpace".to_string(),
        "PageUp" => "Page_Up".to_string(),
        "PageDown" => "Page_Down".to_string(),
        "-" => "minus".to_string(),
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_alphabetic() => {
            one.to_ascii_lowercase()
        }
        other
            if other
                .chars()
                .all(|character| character.is_ascii_alphanumeric()) =>
        {
            other.to_string()
        }
        _ => return Err(HotkeyError::InvalidAccelerator(accelerator.raw.clone())),
    };
    converted.push(&key);
    Ok(converted.join("+"))
}

fn shortcut_properties(
    accelerator: &Accelerator,
) -> Result<HashMap<String, Value<'static>>, HotkeyError> {
    let mut properties = HashMap::new();
    properties.insert(
        "preferred_trigger".to_string(),
        Value::Str(accelerator_string(accelerator)?.into()),
    );
    properties.insert("description".to_string(), Value::Str("Claim Panel".into()));
    Ok(properties)
}

fn request_path(conn: &zbus::Connection, token: &str) -> Result<OwnedObjectPath, HotkeyError> {
    let sender = conn
        .unique_name()
        .ok_or_else(|| HotkeyError::DbusError("session bus has no unique name".into()))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
    .map_err(|error| HotkeyError::DbusError(format!("request path: {error}")))
}

fn decode_response(signal: &zbus::Message) -> Result<HashMap<String, OwnedValue>, HotkeyError> {
    let (response, results): (u32, HashMap<String, OwnedValue>) = signal
        .body()
        .deserialize()
        .map_err(|error| HotkeyError::DbusError(format!("deserialize Response: {error}")))?;
    match response {
        0 => Ok(results),
        1 => Err(HotkeyError::DbusError("user cancelled".into())),
        _ => Err(HotkeyError::DbusError(format!(
            "portal request failed: response={response}"
        ))),
    }
}

fn decode_session_handle(
    results: &HashMap<String, OwnedValue>,
) -> Result<OwnedObjectPath, HotkeyError> {
    let value = results
        .get("session_handle")
        .ok_or_else(|| HotkeyError::DbusError("portal did not return session_handle".into()))?;
    let path = value.downcast_ref::<&str>().map_err(|error| {
        HotkeyError::DbusError(format!("session_handle is not a string: {error}"))
    })?;
    OwnedObjectPath::try_from(path)
        .map_err(|error| HotkeyError::DbusError(format!("invalid session_handle: {error}")))
}

async fn create_session(conn: &zbus::Connection) -> Result<OwnedObjectPath, HotkeyError> {
    let proxy = zbus::Proxy::new(conn, SERVICE, PATH, INTERFACE)
        .await
        .map_err(|error| HotkeyError::DbusError(format!("portal proxy: {error}")))?;
    let expected_path = request_path(conn, "dormant_tray_create")?;
    let request_proxy = zbus::Proxy::new(conn, SERVICE, &expected_path, REQUEST_INTERFACE)
        .await
        .map_err(|error| HotkeyError::DbusError(format!("request proxy: {error}")))?;
    let mut responses = request_proxy
        .receive_signal("Response")
        .await
        .map_err(|error| HotkeyError::DbusError(format!("signal subscribe: {error}")))?;

    let mut options = HashMap::new();
    options.insert(
        "handle_token".to_string(),
        Value::Str("dormant_tray_create".into()),
    );
    options.insert(
        "session_handle_token".to_string(),
        Value::Str("dormant_tray_claim".into()),
    );
    let returned_path: OwnedObjectPath = proxy
        .call("CreateSession", &(options,))
        .await
        .map_err(|error| HotkeyError::DbusError(format!("CreateSession: {error}")))?;
    if returned_path != expected_path {
        return Err(HotkeyError::DbusError(format!(
            "CreateSession returned unexpected request path {returned_path}"
        )));
    }
    let response = responses.next().await.ok_or_else(|| {
        HotkeyError::DbusError("portal request stream ended before response".into())
    })?;
    let session = decode_session_handle(&decode_response(&response)?)?;
    debug!(%session, "portal session created");
    Ok(session)
}

async fn bind_shortcut(
    conn: &zbus::Connection,
    session: &ObjectPath<'_>,
    accelerator: &Accelerator,
) -> Result<(), HotkeyError> {
    let proxy = zbus::Proxy::new(conn, SERVICE, PATH, INTERFACE)
        .await
        .map_err(|error| HotkeyError::DbusError(format!("portal proxy: {error}")))?;
    let expected_path = request_path(conn, "dormant_tray_bind")?;
    let request_proxy = zbus::Proxy::new(conn, SERVICE, &expected_path, REQUEST_INTERFACE)
        .await
        .map_err(|error| HotkeyError::DbusError(format!("request proxy: {error}")))?;
    let mut responses = request_proxy
        .receive_signal("Response")
        .await
        .map_err(|error| HotkeyError::DbusError(format!("signal subscribe: {error}")))?;

    let shortcuts = vec![("claim_panel".to_string(), shortcut_properties(accelerator)?)];
    let mut options = HashMap::new();
    options.insert(
        "handle_token".to_string(),
        Value::Str("dormant_tray_bind".into()),
    );
    let returned_path: OwnedObjectPath = proxy
        .call("BindShortcuts", &(session, shortcuts, "", options))
        .await
        .map_err(|error| {
            if error.to_string().contains("shortcut") || error.to_string().contains("invalid") {
                HotkeyError::InvalidAccelerator(accelerator.raw.clone())
            } else {
                HotkeyError::DbusError(format!("BindShortcuts: {error}"))
            }
        })?;
    if returned_path != expected_path {
        return Err(HotkeyError::DbusError(format!(
            "BindShortcuts returned unexpected request path {returned_path}"
        )));
    }
    let response = responses.next().await.ok_or_else(|| {
        HotkeyError::DbusError("portal request stream ended before response".into())
    })?;
    let _results = decode_response(&response)?;
    debug!(shortcut = %accelerator, "shortcut bound through portal");
    Ok(())
}

#[async_trait]
impl HotkeyRegistrar for PortalHotkeyRegistrar {
    async fn register_claim(
        &mut self,
        accelerator: &Accelerator,
        target: &str,
        arm: bool,
        tx: UnboundedSender<Action>,
    ) -> Result<(), HotkeyError> {
        self.unregister_claim().await;
        let conn = zbus::Connection::session()
            .await
            .map_err(|error| HotkeyError::DbusError(format!("session bus: {error}")))?;
        let session = create_session(&conn).await?;
        bind_shortcut(&conn, &session, accelerator).await?;
        let target = target.to_string();
        let listener_conn = conn.clone();
        let listener_session = session.clone();
        let signal_task = tokio::spawn(async move {
            if let Err(error) =
                listen_for_activation(listener_conn, listener_session.into(), &target, arm, tx)
                    .await
            {
                warn!(error = %error, "portal activation listener exited");
            }
        });
        self.conn = Some(conn);
        self.session = Some(session);
        self.signal_task = Some(signal_task);
        Ok(())
    }

    async fn unregister_claim(&mut self) {
        if let Some(task) = self.signal_task.take() {
            task.abort();
        }
        self.conn = None;
        self.session = None;
    }
}

async fn listen_for_activation(
    conn: zbus::Connection,
    session: ObjectPath<'static>,
    target: &str,
    arm: bool,
    tx: UnboundedSender<Action>,
) -> Result<(), HotkeyError> {
    let proxy = zbus::Proxy::new(&conn, SERVICE, PATH, INTERFACE)
        .await
        .map_err(|error| HotkeyError::DbusError(format!("portal proxy: {error}")))?;
    let mut stream = proxy
        .receive_signal("Activated")
        .await
        .map_err(|error| HotkeyError::DbusError(format!("signal subscribe: {error}")))?;
    while let Some(signal) = stream.next().await {
        let body = signal.body();
        let (message_session, shortcut_id, _timestamp, _options): (
            ObjectPath<'_>,
            String,
            u64,
            HashMap<String, Value>,
        ) = match body.deserialize() {
            Ok(value) => value,
            Err(error) => {
                warn!(error = %error, "ignored malformed portal Activated signal");
                continue;
            }
        };
        if message_session.as_str() != session.as_str() || shortcut_id != "claim_panel" {
            continue;
        }
        if tx.send(claim_action(target, arm)).is_err() {
            break;
        }
        debug!(%target, arm, "claim hotkey activated via portal");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerator_conversion_matches_xdg_wire_format() {
        assert_eq!(
            accelerator_string(&Accelerator::parse("Meta+F12").unwrap()).unwrap(),
            "LOGO+F12"
        );
        assert_eq!(
            accelerator_string(&Accelerator::parse("Control+Alt+Space").unwrap()).unwrap(),
            "CTRL+ALT+space"
        );
    }

    #[test]
    fn protocol_decodes_string_session_and_uses_preferred_trigger() {
        let mut results = HashMap::new();
        results.insert(
            "session_handle".to_string(),
            OwnedValue::try_from(Value::Str(
                "/org/freedesktop/portal/desktop/session/1_42/dormant".into(),
            ))
            .unwrap(),
        );
        assert_eq!(
            decode_session_handle(&results).unwrap().as_str(),
            "/org/freedesktop/portal/desktop/session/1_42/dormant"
        );
        let shortcut = shortcut_properties(&Accelerator::parse("Meta+F12").unwrap()).unwrap();
        assert_eq!(
            shortcut
                .get("preferred_trigger")
                .unwrap()
                .downcast_ref::<&str>()
                .unwrap(),
            "LOGO+F12"
        );
        assert!(!shortcut.contains_key("shortcut"));
    }

    #[test]
    fn drop_without_registration_does_not_panic() {
        drop(PortalHotkeyRegistrar::new());
    }
}
