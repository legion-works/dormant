//! `dormantctl pause` / `dormantctl resume` — control blanking pauses.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};

use crate::client;

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
    handle_response(&resp)
}

/// Run the `resume` command.
///
/// # Errors
///
/// Propagates connection and I/O errors.
pub fn run_resume(socket_path: &Path, rule: Option<String>) -> Result<()> {
    let resp = client::send_request(socket_path, &IpcRequest::Resume { rule })?;
    handle_response(&resp)
}

fn handle_response(resp: &IpcResponse) -> Result<()> {
    if resp.ok {
        println!("ok");
        Ok(())
    } else {
        anyhow::bail!("{}", resp.error.as_deref().unwrap_or("unknown error"))
    }
}
