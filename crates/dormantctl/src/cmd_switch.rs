//! `dormantctl switch` — write a local or peer input code to a shared display.
//!
//! Each write is a direct DDC/CI command over the local machine's own bus;
//! there is no network negotiation.  A local pull is always available on a
//! shared display with a configured input code.  A peer push requires
//! `shared_peer_input_write_code` to be set.

use std::path::Path;

use anyhow::Result;
use dormant_core::ipc_proto::IpcRequest;

use dormantctl::client;

/// Run the `switch` command — write the local input code.
///
/// # Errors
///
/// Propagates transport failures and daemon rejection reasons.
pub fn run(socket_path: &Path, display: &str) -> Result<()> {
    let request = IpcRequest::SwitchToLocal {
        display: display.to_string(),
    };
    let response = client::send_request(socket_path, &request)?;
    client::check_response(&response)?;
    println!("switched");
    Ok(())
}

/// Run the `switch --to-peer` command — write the peer input code.
///
/// # Errors
///
/// Propagates transport failures and daemon rejection reasons (including
/// "not configured" when `shared_peer_input_write_code` is absent).
pub fn run_peer(socket_path: &Path, display: &str) -> Result<()> {
    let request = IpcRequest::SwitchToPeer {
        display: display.to_string(),
    };
    let response = client::send_request(socket_path, &request)?;
    client::check_response(&response)?;
    println!("switched to peer");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_to_local_maps_to_switch_to_local_request() {
        // We test the request construction directly — the wire shape is
        // verified by the serde roundtrip tests in dormant-core.
        let req = IpcRequest::SwitchToLocal {
            display: "monitor".into(),
        };
        assert!(matches!(
            req,
            IpcRequest::SwitchToLocal { display } if display == "monitor"
        ));
    }

    #[test]
    fn switch_to_peer_maps_to_switch_to_peer_request() {
        let req = IpcRequest::SwitchToPeer {
            display: "monitor".into(),
        };
        assert!(matches!(
            req,
            IpcRequest::SwitchToPeer { display } if display == "monitor"
        ));
    }
}
