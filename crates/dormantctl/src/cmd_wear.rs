//! Active-sampling consent commands.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use dormant_core::ipc_proto::{IpcRequest, WearSamplingStatus};

/// Run `wear enable-sampling`, waiting for the daemon's terminal flow result.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_enable(socket: &Path) -> Result<()> {
    let response = send_request_timeout(socket, IpcRequest::WearSamplingEnable)?;
    print_status(response.wear_sampling)
}

/// Run `wear disable-sampling`, optionally deleting the consent record.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_disable(socket: &Path, forget: bool) -> Result<()> {
    let response = send_request_timeout(socket, IpcRequest::WearSamplingDisable { forget })?;
    print_status(response.wear_sampling)
}

fn send_request_timeout(
    socket: &Path,
    request: IpcRequest,
) -> Result<dormant_core::ipc_proto::IpcResponse> {
    let socket = socket.to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::client::send_request(&socket, &request));
    });
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!("wear sampling request timed out")),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("wear sampling request failed")),
    }
}

fn print_status(status: Option<WearSamplingStatus>) -> Result<()> {
    let status = status.ok_or_else(|| anyhow!("daemon returned no wear sampling status"))?;
    match status {
        WearSamplingStatus::AwaitingConsent => {
            println!("awaiting_consent");
            Ok(())
        }
        WearSamplingStatus::Granted => {
            println!("granted");
            Ok(())
        }
        WearSamplingStatus::Denied => {
            println!("denied");
            Err(anyhow!("wear sampling consent denied"))
        }
        WearSamplingStatus::TimedOut => {
            println!("timed_out");
            Err(anyhow!("wear sampling consent timed out"))
        }
        WearSamplingStatus::Error(reason) => {
            println!("error({reason})");
            Err(anyhow!("wear sampling failed: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::print_status;
    use dormant_core::ipc_proto::WearSamplingStatus;

    #[test]
    fn terminal_error_status_returns_nonzero_result() {
        assert!(
            print_status(Some(WearSamplingStatus::Error(
                "wear_sampling_wrong_monitor".to_owned(),
            )))
            .is_err()
        );
    }

    #[test]
    fn granted_status_returns_success() {
        assert!(print_status(Some(WearSamplingStatus::Granted)).is_ok());
    }
}
