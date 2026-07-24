//! `dormantctl switch` — request or arm a shared-panel claim.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use dormant_core::ipc_proto::{ClaimSharedResultWire, IpcRequest};

use dormantctl::client;

/// Build the IPC request for a switch command.
#[must_use]
pub(crate) fn request_for(display: &str, arm: bool) -> IpcRequest {
    if arm {
        IpcRequest::ClaimArm {
            display: display.to_string(),
        }
    } else {
        IpcRequest::ClaimShared {
            display: display.to_string(),
        }
    }
}

fn format_deadline(deadline_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    let seconds = deadline_ms.saturating_sub(now_ms).div_ceil(1_000);
    format!("in {seconds}s")
}

/// Run the `switch` command.
///
/// # Errors
///
/// Propagates transport failures and daemon rejection reasons.
pub fn run(socket_path: &Path, display: &str, arm: bool) -> Result<()> {
    let response = client::send_request(socket_path, &request_for(display, arm))?;
    client::check_response(&response)?;

    if arm {
        let result = response
            .claim_arm
            .ok_or_else(|| anyhow::anyhow!("daemon returned no arm result"))?;
        if result.armed {
            println!("armed {}", format_deadline(result.deadline_ms));
            Ok(())
        } else {
            anyhow::bail!(result.reason.unwrap_or_else(|| "arm rejected".to_string()))
        }
    } else {
        match response
            .claim_shared
            .ok_or_else(|| anyhow::anyhow!("daemon returned no claim result"))?
        {
            ClaimSharedResultWire::Accepted { deadline_ms } => {
                println!("accepted {}", format_deadline(deadline_ms));
                Ok(())
            }
            ClaimSharedResultWire::Busy => anyhow::bail!("busy"),
            ClaimSharedResultWire::Denied { reason } | ClaimSharedResultWire::Failed { reason } => {
                anyhow::bail!(reason)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_arm_maps_to_claim_arm() {
        assert!(matches!(
            request_for("monitor", true),
            IpcRequest::ClaimArm { display } if display == "monitor"
        ));
    }

    #[test]
    fn deadline_is_humanized_relative_to_now() {
        let now_ms: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        let rendered = format_deadline(now_ms + 5_000);
        assert!(rendered == "in 5s" || rendered == "in 6s", "{rendered}");
    }

    #[test]
    fn switch_plain_maps_to_claim_shared() {
        assert!(matches!(
            request_for("monitor", false),
            IpcRequest::ClaimShared { display } if display == "monitor"
        ));
    }
}
