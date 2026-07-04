//! `dormantctl blank` / `dormantctl wake` — force blank/wake a display.

use std::path::Path;

use anyhow::Result;
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};

use crate::client;

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
    handle_response(&resp)
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
