//! Pure tray-action planning plus injected platform I/O execution.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::rules::StateSnapshot;
use dormantctl::client;
use tracing::info;

use crate::menu::Action;

/// The side effects required to carry out a tray action.
#[derive(Debug, Clone)]
pub enum DispatchPlan {
    /// Do not perform any action.
    Ignore,
    /// Send one or more IPC requests to the daemon.
    Ipc(Vec<IpcRequest>),
    /// Open the daemon web UI on this local port.
    OpenWeb(u16),
    /// Request that the platform tray exits.
    Quit,
}

/// Convert a menu action into an I/O-free execution plan.
#[must_use]
pub fn plan_action(
    action: &Action,
    snapshot: Option<&StateSnapshot>,
    unreachable: bool,
) -> DispatchPlan {
    if unreachable && !matches!(action, Action::OpenWebUi { .. } | Action::Quit) {
        return DispatchPlan::Ignore;
    }

    match action {
        Action::Pause(duration) => DispatchPlan::Ipc(vec![IpcRequest::Pause {
            rule: None,
            duration_s: duration.map(|value| value.as_secs()),
        }]),
        Action::Resume => DispatchPlan::Ipc(vec![IpcRequest::Resume { rule: None }]),
        Action::BlankAll => DispatchPlan::Ipc(snapshot.map_or_else(Vec::new, |state| {
            state
                .displays
                .iter()
                .map(|(id, _)| IpcRequest::Blank {
                    display: id.clone(),
                })
                .collect()
        })),
        Action::WakeAll => DispatchPlan::Ipc(snapshot.map_or_else(Vec::new, |state| {
            state
                .displays
                .iter()
                .map(|(id, _)| IpcRequest::Wake {
                    display: id.clone(),
                })
                .collect()
        })),
        Action::BlankOne(display) => DispatchPlan::Ipc(vec![IpcRequest::Blank {
            display: display.clone(),
        }]),
        Action::WakeOne(display) => DispatchPlan::Ipc(vec![IpcRequest::Wake {
            display: display.clone(),
        }]),
        Action::OpenWebUi { port } => DispatchPlan::OpenWeb(*port),
        Action::Quit => DispatchPlan::Quit,
        Action::Separator => DispatchPlan::Ignore,
    }
}

/// Platform operations needed by [`execute_plan`].
pub trait DispatchCapabilities: Send + Sync + 'static {
    /// Send one request to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or daemon request fails.
    fn send_ipc(&self, socket: &Path, request: &IpcRequest) -> Result<()>;

    /// Open the local web UI.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform browser opener cannot be started.
    fn open_web(&self, port: u16) -> Result<()>;

    /// Request that the tray exits.
    fn request_quit(&self);
}

/// Concrete capabilities used by the Linux tray frontend.
pub struct SystemCapabilities {
    request_quit: Arc<dyn Fn() + Send + Sync>,
}

impl SystemCapabilities {
    /// Create capabilities whose quit operation calls `request_quit`.
    #[must_use]
    pub fn new(request_quit: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { request_quit }
    }
}

impl DispatchCapabilities for SystemCapabilities {
    fn send_ipc(&self, socket: &Path, request: &IpcRequest) -> Result<()> {
        let response = client::send_request(socket, request)?;
        if !response.ok {
            anyhow::bail!(
                "daemon returned error: {}",
                response.error.as_deref().unwrap_or("unknown")
            );
        }
        Ok(())
    }

    fn open_web(&self, port: u16) -> Result<()> {
        let url = format!("http://127.0.0.1:{port}");
        #[cfg(target_os = "linux")]
        let opener = "xdg-open";
        #[cfg(target_os = "macos")]
        let opener = "open";

        std::process::Command::new(opener).arg(&url).spawn()?;
        info!(%url, "opened web UI");
        Ok(())
    }

    fn request_quit(&self) {
        (self.request_quit)();
    }
}

/// Execute a plan through the supplied platform capabilities.
///
/// # Errors
///
/// Returns an error when the daemon rejects an IPC request or a platform
/// operation fails.
pub async fn execute_plan(
    plan: DispatchPlan,
    socket: PathBuf,
    capabilities: Arc<dyn DispatchCapabilities>,
) -> Result<()> {
    match plan {
        DispatchPlan::Ignore => Ok(()),
        DispatchPlan::Ipc(requests) => {
            tokio::task::spawn_blocking(move || {
                for request in requests {
                    capabilities.send_ipc(&socket, &request)?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            Ok(())
        }
        DispatchPlan::OpenWeb(port) => capabilities.open_web(port),
        DispatchPlan::Quit => {
            capabilities.request_quit();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::{DispatchCapabilities, DispatchPlan, execute_plan, plan_action};
    use crate::menu::Action;
    use dormant_core::ipc_proto::IpcRequest;
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};

    fn display(id: &str) -> (String, DisplaySnapshot) {
        (
            id.into(),
            DisplaySnapshot {
                phase: "active".into(),
                inhibited: false,
                paused: false,
                cmd_gen: 0,
                scope: dormant_core::config::DisplayScope::Private,
                owned: true,
                observed_input_code: None,
                panel_state: None,
                controllers: vec![],
                wake_attempts: 0,
                last_blank_failed: false,
                stage: None,
            },
        )
    }

    fn two_display_snapshot() -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![display("a"), display("b")],
            pending_reload: None,
            rollback: None,
        }
    }

    #[test]
    fn blank_all_fans_out_to_snapshot_displays() {
        let DispatchPlan::Ipc(requests) =
            plan_action(&Action::BlankAll, Some(&two_display_snapshot()), false)
        else {
            panic!("expected IPC plan")
        };
        assert_eq!(
            serde_json::to_value(requests).unwrap(),
            serde_json::json!([
                {"req": "blank", "display": "a"},
                {"req": "blank", "display": "b"}
            ])
        );
    }

    #[test]
    fn unreachable_suppresses_mutation_but_not_open_or_quit() {
        assert!(matches!(
            plan_action(&Action::Resume, None, true),
            DispatchPlan::Ignore
        ));
        assert!(matches!(
            plan_action(&Action::OpenWebUi { port: 8137 }, None, true),
            DispatchPlan::OpenWeb(8137)
        ));
        assert!(matches!(
            plan_action(&Action::Quit, None, true),
            DispatchPlan::Quit
        ));
    }

    #[derive(Default)]
    struct MockCapabilities {
        requests: Mutex<Vec<IpcRequest>>,
        ports: Mutex<Vec<u16>>,
        quits: Mutex<usize>,
    }

    impl DispatchCapabilities for MockCapabilities {
        fn send_ipc(&self, _: &Path, request: &IpcRequest) -> anyhow::Result<()> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(())
        }

        fn open_web(&self, port: u16) -> anyhow::Result<()> {
            self.ports.lock().unwrap().push(port);
            Ok(())
        }

        fn request_quit(&self) {
            *self.quits.lock().unwrap() += 1;
        }
    }

    #[tokio::test]
    async fn execute_plan_dispatches_in_order_and_invokes_side_effects_once() {
        let capabilities = Arc::new(MockCapabilities::default());
        let ipc_capabilities = capabilities.clone();
        execute_plan(
            DispatchPlan::Ipc(vec![
                IpcRequest::Blank {
                    display: "a".into(),
                },
                IpcRequest::Wake {
                    display: "b".into(),
                },
            ]),
            "/tmp/dormant.sock".into(),
            ipc_capabilities,
        )
        .await
        .unwrap();
        execute_plan(
            DispatchPlan::OpenWeb(8137),
            "/tmp/dormant.sock".into(),
            capabilities.clone(),
        )
        .await
        .unwrap();
        execute_plan(
            DispatchPlan::Quit,
            "/tmp/dormant.sock".into(),
            capabilities.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            serde_json::to_value(capabilities.requests.lock().unwrap().as_slice()).unwrap(),
            serde_json::json!([
                {"req": "blank", "display": "a"},
                {"req": "wake", "display": "b"}
            ])
        );
        assert_eq!(*capabilities.ports.lock().unwrap(), vec![8137]);
        assert_eq!(*capabilities.quits.lock().unwrap(), 1);
    }
}
