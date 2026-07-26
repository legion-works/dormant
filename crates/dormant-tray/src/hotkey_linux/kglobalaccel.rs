//! Direct KDE `KGlobalAccel` registration.

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, warn};
use zbus::zvariant::OwnedObjectPath;

use super::{accelerator_tokens, switch_to_local_action};
use crate::hotkey::{Accelerator, HotkeyError, HotkeyRegistrar};
use crate::menu::Action;

const SERVICE: &str = "org.kde.kglobalaccel";
const PATH: &str = "/kglobalaccel";
const INTERFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_INTERFACE: &str = "org.kde.kglobalaccel.Component";
const COMPONENT: &str = "dormant-tray";
const ACTION: &str = "_k_session:claim_panel";
const SET_PRESENT_NO_AUTOLOAD: u32 = 6;

pub(super) struct KGlobalAccelHotkeyRegistrar {
    conn: Option<zbus::Connection>,
    action: Option<(String, String)>,
    signal_task: Option<tokio::task::JoinHandle<()>>,
}

impl KGlobalAccelHotkeyRegistrar {
    pub(super) const fn new() -> Self {
        Self {
            conn: None,
            action: None,
            signal_task: None,
        }
    }
}

fn accelerator_code(accelerator: &Accelerator) -> Result<i32, HotkeyError> {
    let (modifiers, key) = accelerator_tokens(accelerator)?;
    let modifier_bits = modifiers.iter().fold(0_u32, |bits, modifier| {
        bits | match *modifier {
            "Shift" => 0x0200_0000,
            "Control" => 0x0400_0000,
            "Alt" => 0x0800_0000,
            "Meta" | "Super" => 0x1000_0000,
            _ => 0,
        }
    });
    let key_code =
        key_code(key).ok_or_else(|| HotkeyError::InvalidAccelerator(accelerator.raw.clone()))?;
    i32::try_from(modifier_bits | key_code)
        .map_err(|_| HotkeyError::InvalidAccelerator(accelerator.raw.clone()))
}

fn key_code(key: &str) -> Option<u32> {
    if let Some(number) = key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
        && (1..=35).contains(&number)
    {
        return Some(0x0100_0030 + number - 1);
    }
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        return match byte {
            b'a'..=b'z' => Some(u32::from(byte.to_ascii_uppercase())),
            b'A'..=b'Z' | b'0'..=b'9' | b'-' => Some(u32::from(byte)),
            _ => None,
        };
    }
    match key {
        "Space" => Some(0x20),
        "Escape" => Some(0x0100_0000),
        "Tab" => Some(0x0100_0001),
        "Backspace" => Some(0x0100_0003),
        "Return" => Some(0x0100_0004),
        "Enter" => Some(0x0100_0005),
        "Insert" => Some(0x0100_0006),
        "Delete" => Some(0x0100_0007),
        "Home" => Some(0x0100_0010),
        "End" => Some(0x0100_0011),
        "Left" => Some(0x0100_0012),
        "Up" => Some(0x0100_0013),
        "Right" => Some(0x0100_0014),
        "Down" => Some(0x0100_0015),
        "PageUp" => Some(0x0100_0016),
        "PageDown" => Some(0x0100_0017),
        _ => None,
    }
}

fn action_id() -> Vec<String> {
    vec![
        COMPONENT.to_string(),
        ACTION.to_string(),
        "dormant".to_string(),
        "Claim panel".to_string(),
    ]
}

async fn unregister_action(
    conn: &zbus::Connection,
    component: &str,
    action: &str,
) -> Result<(), HotkeyError> {
    let proxy = zbus::Proxy::new(conn, SERVICE, PATH, INTERFACE)
        .await
        .map_err(|error| HotkeyError::DbusError(format!("KGlobalAccel proxy: {error}")))?;
    let _removed: bool = proxy
        .call("unregister", &(component, action))
        .await
        .map_err(|error| HotkeyError::DbusError(format!("KGlobalAccel unregister: {error}")))?;
    Ok(())
}

#[async_trait]
impl HotkeyRegistrar for KGlobalAccelHotkeyRegistrar {
    async fn register_claim(
        &mut self,
        accelerator: &Accelerator,
        target: &str,
        tx: UnboundedSender<Action>,
    ) -> Result<(), HotkeyError> {
        self.unregister_claim().await;
        let key = accelerator_code(accelerator)?;
        let conn = zbus::Connection::session()
            .await
            .map_err(|error| HotkeyError::DbusError(format!("session bus: {error}")))?;
        let proxy = zbus::Proxy::new(&conn, SERVICE, PATH, INTERFACE)
            .await
            .map_err(|error| HotkeyError::DbusError(format!("KGlobalAccel proxy: {error}")))?;
        let action_id = action_id();
        let registered: Result<(), HotkeyError> = async {
            let (): () = proxy
                .call("doRegister", &(action_id.clone(),))
                .await
                .map_err(|error| {
                    HotkeyError::DbusError(format!("KGlobalAccel doRegister: {error}"))
                })?;
            let component_path: OwnedObjectPath = proxy
                .call("getComponent", &(COMPONENT,))
                .await
                .map_err(|error| {
                HotkeyError::DbusError(format!("KGlobalAccel getComponent: {error}"))
            })?;
            let (ready_tx, ready_rx) = oneshot::channel();
            let listener_conn = conn.clone();
            let listener_target = target.to_string();
            let signal_task = tokio::spawn(async move {
                if let Err(error) = listen_for_activation(
                    listener_conn,
                    component_path,
                    &listener_target,
                    tx,
                    ready_tx,
                )
                .await
                {
                    warn!(error = %error, "KGlobalAccel activation listener exited");
                }
            });
            ready_rx.await.map_err(|_| {
                HotkeyError::DbusError("KGlobalAccel listener exited during setup".into())
            })??;

            let accepted: Vec<i32> = proxy
                .call(
                    "setShortcut",
                    &(action_id.clone(), vec![key], SET_PRESENT_NO_AUTOLOAD),
                )
                .await
                .map_err(|error| {
                    HotkeyError::DbusError(format!("KGlobalAccel setShortcut: {error}"))
                })?;
            if accepted != vec![key] {
                signal_task.abort();
                return Err(HotkeyError::DbusError(format!(
                    "KGlobalAccel rejected accelerator {accelerator}"
                )));
            }
            self.signal_task = Some(signal_task);
            Ok(())
        }
        .await;

        if let Err(error) = registered {
            let _ = unregister_action(&conn, COMPONENT, ACTION).await;
            return Err(error);
        }
        self.conn = Some(conn);
        self.action = Some((COMPONENT.into(), ACTION.into()));
        debug!(%accelerator, "claim shortcut registered through KGlobalAccel");
        Ok(())
    }

    async fn unregister_claim(&mut self) {
        if let Some(task) = self.signal_task.take() {
            task.abort();
        }
        if let (Some(conn), Some((component, action))) = (&self.conn, self.action.take())
            && let Err(error) = unregister_action(conn, &component, &action).await
        {
            warn!(error = %error, "failed to unregister KGlobalAccel shortcut");
        }
        self.conn = None;
    }
}

async fn listen_for_activation(
    conn: zbus::Connection,
    component_path: OwnedObjectPath,
    target: &str,
    tx: UnboundedSender<Action>,
    ready: oneshot::Sender<Result<(), HotkeyError>>,
) -> Result<(), HotkeyError> {
    let proxy = match zbus::Proxy::new(&conn, SERVICE, component_path, COMPONENT_INTERFACE).await {
        Ok(proxy) => proxy,
        Err(error) => {
            let message = HotkeyError::DbusError(format!("KGlobalAccel component proxy: {error}"));
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };
    let mut stream = match proxy.receive_signal("globalShortcutPressed").await {
        Ok(stream) => stream,
        Err(error) => {
            let message = HotkeyError::DbusError(format!("KGlobalAccel signal subscribe: {error}"));
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };
    let _ = ready.send(Ok(()));

    while let Some(signal) = stream.next().await {
        let Ok((component, action, _timestamp)) =
            signal.body().deserialize::<(String, String, i64)>()
        else {
            continue;
        };
        if component != COMPONENT || action != ACTION {
            continue;
        }
        if tx.send(switch_to_local_action(target)).is_err() {
            break;
        }
        debug!(%target, "switch hotkey activated through KGlobalAccel");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerator_conversion_matches_qt_wire_format() {
        assert_eq!(
            accelerator_code(&Accelerator::parse("Meta+F12").unwrap()).unwrap(),
            0x1100_003b
        );
        assert_eq!(
            accelerator_code(&Accelerator::parse("Control+Alt+Space").unwrap()).unwrap(),
            0x0c00_0020
        );
    }

    #[test]
    fn drop_without_registration_does_not_panic() {
        drop(KGlobalAccelHotkeyRegistrar::new());
    }
}
