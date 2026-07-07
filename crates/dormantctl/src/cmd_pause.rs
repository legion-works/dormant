//! `dormantctl pause` / `dormantctl resume` — control blanking pauses.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use dormant_core::ipc_proto::IpcRequest;

use dormantctl::client;

/// Run the `pause` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run_pause(
    socket_path: &Path,
    duration: Option<Duration>,
    rule: Option<String>,
) -> Result<()> {
    let duration_s = duration.map(|d| d.as_secs());
    let resp = client::send_request(socket_path, &IpcRequest::Pause { rule, duration_s })?;
    client::check_response(&resp)
}

/// Run the `resume` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run_resume(socket_path: &Path, rule: Option<String>) -> Result<()> {
    let resp = client::send_request(socket_path, &IpcRequest::Resume { rule })?;
    client::check_response(&resp)
}
