//! Doctor report types for the probe system.
//!
//! Wire-ready report types used by the doctor CLI, the daemon IPC, and the web
//! UI.  These live in `dormant-core` so the daemon IPC layer and web UI can
//! reference them without a core→doctor dependency cycle.
//!
//! The `dormant-doctor` crate re-exports these types; internal probe logic
//! lives there.

use serde::{Deserialize, Serialize};

/// The status of a single health check.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Probe succeeded.
    Ok,
    /// Probe failed.
    Fail,
    /// Probe was skipped (no applicable config).
    Skip,
    /// The probe is not supported on this platform or in this release.
    NotSupported,
}

/// A single health check result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Check {
    /// Human-readable probe name (e.g. `"ddcci"`, `"usb /dev/ttyUSB0"`).
    pub name: String,
    /// Probe status.
    pub status: CheckStatus,
    /// Optional additional detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Container for a full set of health check results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorReport {
    /// All checks performed.
    pub checks: Vec<Check>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_not_supported_serde_snake_case() {
        let c = Check {
            name: "kwin-dpms".into(),
            status: CheckStatus::NotSupported,
            detail: Some("not yet implemented".into()),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            json.contains("not_supported"),
            "expected snake_case 'not_supported': {json}"
        );
    }

    #[test]
    fn check_skip_serde_no_detail() {
        let c = Check {
            name: "mqtt".into(),
            status: CheckStatus::Skip,
            detail: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""skip""#), "expected 'skip': {json}");
        assert!(
            !json.contains("detail"),
            "None detail should be absent: {json}"
        );
    }

    #[test]
    fn doctor_report_empty_checks_serde() {
        let report = DoctorReport { checks: vec![] };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(json, r#"{"checks":[]}"#);
    }
}
