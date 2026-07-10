//! DDC/CI display probe — enumerates displays over I²C.
//!
//! Only available on Linux where `ddc-hi` is supported.

#[cfg(target_os = "linux")]
use crate::types::ProbeResult;
#[cfg(target_os = "linux")]
use dormant_displays::ddc_lock::PanelLocks;
#[cfg(target_os = "linux")]
use dormant_displays::vcp_ops::RealVcp;
#[cfg(target_os = "linux")]
use dormant_displays::vcp_ops::{VcpOps, VcpPriority};

/// Probe DDC/CI-capable displays.
#[cfg(target_os = "linux")]
pub async fn probe_ddcci() -> ProbeResult {
    let ops = RealVcp;
    let displays = ops.list_displays().await;

    if displays.is_empty() {
        return ProbeResult::fail("ddcci", "no DDC/CI displays detected");
    }

    // This probe is a standalone, one-shot `dormantctl doctor` diagnostic —
    // it does not share the daemon's controller chain or its process-wide
    // `PanelLocks` registry (there is no daemon-side handle to reach into
    // from here). A fresh registry, scoped to this single probe call, still
    // gives every display's read its own per-panel serialization (spec
    // §4.3) for the duration of the loop below, which is all this
    // diagnostic needs. Command priority: an operator-triggered diagnostic
    // read is command-path work, never periodic sampling.
    let panel_locks = PanelLocks::new();

    let mut details: Vec<String> = Vec::new();
    let mut all_ok = true;

    for display in &displays {
        let ident = &display.ident_string;
        let lock = panel_locks.get(ident);
        let brightness = ops.get_vcp(ident, 0x10, &lock, VcpPriority::Command).await;
        let d6 = ops.get_vcp(ident, 0xD6, &lock, VcpPriority::Command).await;

        let mut line = format!("  {ident}: brightness=");
        match brightness {
            Ok(v) => {
                line.push_str(&v.to_string());
            }
            Err(e) => {
                use std::fmt::Write;
                let _ = write!(line, "ERR({e})");
                all_ok = false;
            }
        }
        line.push_str(", power_control=");
        match d6 {
            Ok(_) => line.push_str("supported"),
            Err(_) => line.push_str("not supported"),
        }
        details.push(line);
    }

    let detail = details.join("\n");
    if all_ok {
        ProbeResult::pass("ddcci", detail)
    } else {
        ProbeResult::fail("ddcci", detail)
    }
}
