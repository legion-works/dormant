//! `dormantctl blank` / `dormantctl wake` — force blank/wake a display.

use std::path::Path;

use anyhow::Result;
use dormant_core::ipc_proto::IpcRequest;

use dormantctl::client;

/// Run the `blank` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run_blank(socket_path: &Path, display: &str) -> Result<()> {
    let resp = client::send_request(
        socket_path,
        &IpcRequest::Blank {
            display: display.to_string(),
        },
    )?;
    client::check_response(&resp)
}

/// Run the `wake` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run_wake(socket_path: &Path, display: &str) -> Result<()> {
    let resp = client::send_request(
        socket_path,
        &IpcRequest::Wake {
            display: display.to_string(),
        },
    )?;
    client::check_response(&resp)
}
