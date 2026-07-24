//! Linux global hotkey registration via XDG Desktop Portal
//! [`GlobalShortcuts`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html).
//!
//! The portal bridges to the desktop's native shortcut daemon
//! (`KGlobalAccel` on KDE, GNOME Shell on GNOME) — no desktop-specific
//! D-Bus API is called directly.  If the portal or session bus is
//! unavailable, registration returns
//! [`HotkeyError::DbusError`](crate::hotkey::HotkeyError::DbusError) and
//! the tray keeps the manual "Claim panel" menu path.

#![cfg(target_os = "linux")]
#![allow(missing_docs)]

use std::collections::HashMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::hotkey::{Accelerator, HotkeyError, HotkeyRegistrar};
use crate::menu::Action;

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

/// The portal returns a request object path from method calls; the
/// actual response arrives via the `Response` signal on that object.
/// `response == 0` means success; `1` is user-cancelled; `2` is error.
async fn await_portal_response(
    conn: &zbus::Connection,
    request_path: &ObjectPath<'_>,
) -> Result<HashMap<String, OwnedValue>, HotkeyError> {
    let proxy = zbus::Proxy::new(conn, PORTAL_SERVICE, request_path, REQUEST_IFACE)
        .await
        .map_err(|e| HotkeyError::DbusError(format!("request proxy: {e}")))?;

    let mut stream = proxy
        .receive_signal("Response")
        .await
        .map_err(|e| HotkeyError::DbusError(format!("signal subscribe: {e}")))?;

    let signal = stream.next().await.ok_or_else(|| {
        HotkeyError::DbusError("portal request stream ended before response".into())
    })?;

    let body = signal.body();
    let (response, results): (u32, HashMap<String, OwnedValue>) = body
        .deserialize()
        .map_err(|e| HotkeyError::DbusError(format!("deserialize Response: {e}")))?;

    match response {
        0 => Ok(results),
        1 => Err(HotkeyError::DbusError("user cancelled".into())),
        _ => Err(HotkeyError::DbusError(format!(
            "portal request failed: response={response}"
        ))),
    }
}

/// Create a `GlobalShortcuts` session.  The returned object path is an
/// opaque session handle passed to `BindShortcuts`.
async fn create_session(conn: &zbus::Connection) -> Result<OwnedObjectPath, HotkeyError> {
    let proxy = zbus::Proxy::new(conn, PORTAL_SERVICE, PORTAL_PATH, PORTAL_IFACE)
        .await
        .map_err(|e| HotkeyError::DbusError(format!("portal proxy: {e}")))?;

    // Pass a session_handle_token so we can identify our session.
    let mut opts = HashMap::new();
    opts.insert(
        "session_handle_token".to_string(),
        Value::Str("dormant_tray_claim".into()),
    );

    let request_path: OwnedObjectPath = proxy
        .call("CreateSession", &(opts,))
        .await
        .map_err(|e| HotkeyError::DbusError(format!("CreateSession: {e}")))?;

    let results = await_portal_response(conn, &request_path).await?;

    let session: OwnedObjectPath = results
        .get("session_handle")
        .and_then(|v| v.downcast_ref::<ObjectPath<'_>>().ok())
        .map(OwnedObjectPath::from)
        .ok_or_else(|| HotkeyError::DbusError("portal did not return session_handle".into()))?;

    debug!(%session, "portal session created");
    Ok(session)
}

/// Bind a claim shortcut to the session.  The portal validates the
/// accelerator syntax; an unsupported string returns an error.
async fn bind_shortcut(
    conn: &zbus::Connection,
    session: &ObjectPath<'_>,
    shortcut_str: &str,
) -> Result<(), HotkeyError> {
    let proxy = zbus::Proxy::new(conn, PORTAL_SERVICE, PORTAL_PATH, PORTAL_IFACE)
        .await
        .map_err(|e| HotkeyError::DbusError(format!("portal proxy: {e}")))?;

    let mut props = HashMap::new();
    props.insert("shortcut".to_string(), Value::Str(shortcut_str.into()));
    props.insert("description".to_string(), Value::Str("Claim Panel".into()));

    let shortcuts = vec![("claim_panel".to_string(), props)];

    let request_path: OwnedObjectPath = proxy
        .call(
            "BindShortcuts",
            &(session, shortcuts, "", &HashMap::<&str, Value>::new()),
        )
        .await
        .map_err(|e| {
            if e.to_string().contains("shortcut") || e.to_string().contains("invalid") {
                HotkeyError::InvalidAccelerator(shortcut_str.into())
            } else {
                HotkeyError::DbusError(format!("BindShortcuts: {e}"))
            }
        })?;

    let _results = await_portal_response(conn, &request_path).await?;
    debug!(%shortcut_str, "shortcut bound");
    Ok(())
}

/// Linux [`HotkeyRegistrar`] backed by the XDG Desktop Portal
/// `GlobalShortcuts` interface.
pub struct ZbusHotkeyRegistrar {
    conn: Option<zbus::Connection>,
    session: Option<OwnedObjectPath>,
    signal_task: Option<tokio::task::JoinHandle<()>>,
}

impl ZbusHotkeyRegistrar {
    #[must_use]
    pub fn new() -> Self {
        Self {
            conn: None,
            session: None,
            signal_task: None,
        }
    }
}

impl Default for ZbusHotkeyRegistrar {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HotkeyRegistrar for ZbusHotkeyRegistrar {
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
            .map_err(|e| HotkeyError::DbusError(format!("session bus: {e}")))?;

        // 1. Create a GlobalShortcuts session.
        let session = create_session(&conn).await?;

        // 2. Bind the claim shortcut.
        bind_shortcut(&conn, &session, &accelerator.raw).await?;

        // 3. Spawn a signal listener that watches for Activated.
        let target_owned = target.to_string();
        let conn_clone = conn.clone();
        let session_clone = session.clone();
        let signal_task = tokio::spawn(async move {
            if let Err(e) =
                listen_for_activation(conn_clone, session_clone.into(), &target_owned, arm, tx)
                    .await
            {
                warn!(error = %e, "portal activation listener exited");
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
        // Dropping the connection closes the session implicitly;
        // the portal cleans up shortcuts when the requesting
        // D-Bus client disconnects.
        self.conn = None;
        self.session = None;
    }
}

/// Listen for `Activated` signals on the portal interface.  The
/// signature is `osta{sv}` — session handle (o), shortcut id (s),
/// timestamp (t), options (a{sv}).
async fn listen_for_activation(
    conn: zbus::Connection,
    session: ObjectPath<'static>,
    target: &str,
    arm: bool,
    tx: UnboundedSender<Action>,
) -> Result<(), HotkeyError> {
    let proxy = zbus::Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, PORTAL_IFACE)
        .await
        .map_err(|e| HotkeyError::DbusError(format!("portal proxy: {e}")))?;

    let mut stream = proxy
        .receive_signal("Activated")
        .await
        .map_err(|e| HotkeyError::DbusError(format!("signal subscribe: {e}")))?;

    while let Some(signal) = stream.next().await {
        let body = signal.body();
        // Portal <= 1.18: Activated(session_handle, shortcut_id, timestamp, options)
        // signature osta{sv} → ObjectPath, String, u64, HashMap
        let (msg_session, shortcut_id, _timestamp, _options): (
            ObjectPath<'_>,
            String,
            u64,
            HashMap<String, Value>,
        ) = match body.deserialize() {
            Ok(v) => v,
            // Try the v2 signature oa{sv} (session_handle, options)
            Err(_) => {
                // The shortcut_id is in the options dict for v2
                if let Ok((msg_session, options)) =
                    body.deserialize::<(ObjectPath<'_>, HashMap<String, Value>)>()
                {
                    let id = options
                        .get("shortcut_id")
                        .and_then(|v| v.downcast_ref::<&str>().ok())
                        .unwrap_or("");
                    (msg_session, id.to_string(), 0, options)
                } else {
                    continue;
                }
            }
        };

        if msg_session.as_str() != session.as_str() || shortcut_id != "claim_panel" {
            continue;
        }

        let action = if arm {
            Action::ArmClaim(target.to_string())
        } else {
            Action::ClaimOne(target.to_string())
        };
        if tx.send(action).is_err() {
            break;
        }
        debug!(%target, arm, "claim hotkey activated via portal");
    }

    Ok(())
}

/// Create a new Linux registrar for the XDG Desktop Portal path.
#[must_use]
pub fn create_linux_registrar() -> Box<dyn HotkeyRegistrar> {
    Box::new(ZbusHotkeyRegistrar::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_linux_registrar_returns_valid_object() {
        let registrar = create_linux_registrar();
        drop(registrar);
    }

    #[test]
    fn drop_without_registration_does_not_panic() {
        let registrar = ZbusHotkeyRegistrar::new();
        drop(registrar);
    }
}
