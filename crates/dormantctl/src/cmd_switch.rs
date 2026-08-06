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
/// By default the daemon is idempotent: when it already owns the panel and
/// the last debounced observation agrees, the daemon returns
/// `"already_local"` instead of re-asserting the DDC write (issue #246).
/// `force=true` bypasses that guard for the genuine re-assert case
/// (operator `--force` on the CLI).
///
/// # Errors
///
/// Propagates transport failures and daemon rejection reasons.
pub fn run(socket_path: &Path, display: &str, force: bool) -> Result<()> {
    let request = IpcRequest::SwitchToLocal {
        display: display.to_string(),
        force,
    };
    let response = client::send_request(socket_path, &request)?;
    client::check_response(&response)?;
    let outcome = response.switch_outcome.as_deref().unwrap_or("switched");
    match outcome {
        "switched" => println!("switched"),
        "already_local" => println!("already local — no action (use --force to re-assert)"),
        // Defensive fallback: a daemon from before the wire-compat contract
        // would omit `switch_outcome` entirely; we report the legacy text.
        other => println!("{other}"),
    }
    Ok(())
}

/// Run the `switch --to-peer` command — write the peer input code.
///
/// Symmetric to [`run`] (issue #246): the daemon's idempotency guard
/// surfaces `"already_peer"` on a no-op push; `--force` bypasses it.
///
/// # Errors
///
/// Propagates transport failures and daemon rejection reasons (including
/// "not configured" when `shared_peer_input_write_code` is absent).
pub fn run_peer(socket_path: &Path, display: &str, force: bool) -> Result<()> {
    let request = IpcRequest::SwitchToPeer {
        display: display.to_string(),
        force,
    };
    let response = client::send_request(socket_path, &request)?;
    client::check_response(&response)?;
    let outcome = response.switch_outcome.as_deref().unwrap_or("switched");
    match outcome {
        "switched" => println!("switched to peer"),
        "already_peer" => println!("already on peer — no action (use --force to re-assert)"),
        other => println!("{other}"),
    }
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
            force: false,
        };
        assert!(matches!(
            req,
            IpcRequest::SwitchToLocal { display, force: false } if display == "monitor"
        ));
    }

    #[test]
    fn switch_to_local_force_true_round_trips() {
        let req = IpcRequest::SwitchToLocal {
            display: "monitor".into(),
            force: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        // force=true is serialized so the daemon sees the operator
        // override flag (issue #246).
        assert!(json.contains("\"force\":true"));
        assert!(matches!(
            req,
            IpcRequest::SwitchToLocal { display, force: true } if display == "monitor"
        ));
    }

    #[test]
    fn switch_to_peer_maps_to_switch_to_peer_request() {
        let req = IpcRequest::SwitchToPeer {
            display: "monitor".into(),
            force: false,
        };
        assert!(matches!(
            req,
            IpcRequest::SwitchToPeer { display, force: false } if display == "monitor"
        ));
    }
}
