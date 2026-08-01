//! Active-sampling consent commands.

use std::path::Path;

use anyhow::{Result, anyhow};
use dormant_core::ipc_proto::{IpcRequest, WearSamplingStatus};

/// Run `wear enable-sampling`, waiting for the daemon's terminal flow result.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_enable(socket: &Path) -> Result<()> {
    let response = crate::client::send_request(socket, &IpcRequest::WearSamplingEnable)?;
    print_status(response.wear_sampling)
}

/// Run `wear disable-sampling`, optionally deleting the consent record.
///
/// # Errors
///
/// Returns an error when IPC fails or the daemon reports a non-success status.
pub fn run_disable(socket: &Path, forget: bool) -> Result<()> {
    let response =
        crate::client::send_request(socket, &IpcRequest::WearSamplingDisable { forget })?;
    print_status(response.wear_sampling)
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
