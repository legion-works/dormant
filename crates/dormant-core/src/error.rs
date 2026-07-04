//! Error types and error-code constants for dormant.
//!
//! Every error variant carries a literal `E_*` code string as its message prefix,
//! grep-stable and never `format!`-constructed. The [`DormantError::code`] method
//! maps each variant back to its constant.

// ── Error-code constants ──────────────────────────────────────────────────────

/// Configuration file is syntactically or structurally invalid.
pub const E_CONFIG_INVALID: &str = "E_CONFIG_INVALID";
/// An unknown key was encountered in the configuration.
pub const E_CONFIG_UNKNOWN_KEY: &str = "E_CONFIG_UNKNOWN_KEY";
/// A zone rule creates a dependency cycle.
pub const E_ZONE_CYCLE: &str = "E_ZONE_CYCLE";
/// A zone references a member that does not exist.
pub const E_ZONE_UNKNOWN_MEMBER: &str = "E_ZONE_UNKNOWN_MEMBER";
/// Credentials file has incorrect permissions.
pub const E_CREDS_PERMS: &str = "E_CREDS_PERMS";
/// Required credentials are missing.
pub const E_CREDS_MISSING: &str = "E_CREDS_MISSING";
/// A display does not support the requested blank mode.
pub const E_MODE_UNSUPPORTED: &str = "E_MODE_UNSUPPORTED";
/// A blank command failed.
pub const E_BLANK_FAILED: &str = "E_BLANK_FAILED";
/// A wake command failed.
pub const E_WAKE_FAILED: &str = "E_WAKE_FAILED";
/// A wake-on-reload command failed.
pub const E_RELOAD_WAKE_FAILED: &str = "E_RELOAD_WAKE_FAILED";
/// Home Assistant authentication failure.
pub const E_HA_AUTH: &str = "E_HA_AUTH";
/// I/O error from a sensor source.
pub const E_SENSOR_IO: &str = "E_SENSOR_IO";
/// I/O error from a display controller.
pub const E_DISPLAY_IO: &str = "E_DISPLAY_IO";
/// Inter-process communication error.
pub const E_IPC: &str = "E_IPC";

// ── DormantError ──────────────────────────────────────────────────────────────

/// Top-level error type for the dormant daemon.
///
/// Every variant's `Display` message begins with the matching `E_*` code constant
/// so that log parsers and `dormantctl doctor` can match on the literal prefix.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DormantError {
    /// Configuration file is syntactically or structurally invalid.
    #[error("E_CONFIG_INVALID: {detail}")]
    ConfigInvalid {
        /// Human-readable description of the problem.
        detail: String,
    },

    /// An unknown key was encountered in the configuration.
    #[error("E_CONFIG_UNKNOWN_KEY: unknown key at '{key_path}'")]
    ConfigUnknownKey {
        /// Dot-separated path to the unknown key.
        key_path: String,
    },

    /// A zone rule creates a dependency cycle.
    #[error("E_ZONE_CYCLE: zone cycle involving '{zone}'")]
    ZoneCycle {
        /// Name of the zone involved in the cycle.
        zone: String,
    },

    /// A zone references a member that does not exist.
    #[error("E_ZONE_UNKNOWN_MEMBER: zone '{zone}' references unknown member '{member}'")]
    ZoneUnknownMember {
        /// Name of the zone.
        zone: String,
        /// Name of the unknown member.
        member: String,
    },

    /// Credentials file has incorrect permissions.
    #[error("E_CREDS_PERMS: credentials file '{path}' has unsafe permissions")]
    CredsPerms {
        /// Path to the credentials file.
        path: String,
    },

    /// Required credentials are missing.
    #[error("E_CREDS_MISSING: missing credential '{what}'")]
    CredsMissing {
        /// Description of what credential is missing.
        what: String,
    },

    /// A display does not support the requested blank mode.
    #[error("E_MODE_UNSUPPORTED: display '{display}' does not support mode '{mode}'")]
    ModeUnsupported {
        /// Name of the display controller.
        display: String,
        /// The unsupported blank mode string.
        mode: String,
    },

    /// A blank command failed.
    #[error("E_BLANK_FAILED: {0}")]
    BlankFailed(#[source] crate::types::CmdFailure),

    /// A wake command failed.
    #[error("E_WAKE_FAILED: {0}")]
    WakeFailed(#[source] crate::types::CmdFailure),

    /// A wake-on-reload command failed.
    #[error("E_RELOAD_WAKE_FAILED: failed to wake display '{display}' on reload")]
    ReloadWakeFailed {
        /// Name of the display that could not be woken.
        display: String,
    },

    /// Home Assistant authentication failure.
    #[error("E_HA_AUTH: {detail}")]
    HaAuth {
        /// Authentication error detail.
        detail: String,
    },

    /// I/O error from a sensor source.
    #[error("E_SENSOR_IO: sensor '{source_id}' I/O error: {detail}")]
    SensorIo {
        /// Identifier of the sensor source.
        source_id: String,
        /// Human-readable I/O error detail.
        detail: String,
    },

    /// I/O error from a display controller.
    #[error("E_DISPLAY_IO: controller '{controller}' I/O error: {detail}")]
    DisplayIo {
        /// Name of the display controller.
        controller: String,
        /// Human-readable I/O error detail.
        detail: String,
    },

    /// Inter-process communication error.
    #[error("E_IPC: {detail}")]
    Ipc {
        /// IPC error detail.
        detail: String,
    },
}

impl DormantError {
    /// Return the literal `E_*` code for this error variant.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid { .. } => E_CONFIG_INVALID,
            Self::ConfigUnknownKey { .. } => E_CONFIG_UNKNOWN_KEY,
            Self::ZoneCycle { .. } => E_ZONE_CYCLE,
            Self::ZoneUnknownMember { .. } => E_ZONE_UNKNOWN_MEMBER,
            Self::CredsPerms { .. } => E_CREDS_PERMS,
            Self::CredsMissing { .. } => E_CREDS_MISSING,
            Self::ModeUnsupported { .. } => E_MODE_UNSUPPORTED,
            Self::BlankFailed(_) => E_BLANK_FAILED,
            Self::WakeFailed(_) => E_WAKE_FAILED,
            Self::ReloadWakeFailed { .. } => E_RELOAD_WAKE_FAILED,
            Self::HaAuth { .. } => E_HA_AUTH,
            Self::SensorIo { .. } => E_SENSOR_IO,
            Self::DisplayIo { .. } => E_DISPLAY_IO,
            Self::Ipc { .. } => E_IPC,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CmdFailure;

    #[test]
    fn code_returns_matching_const_for_config_invalid() {
        let err = DormantError::ConfigInvalid {
            detail: "bad syntax".into(),
        };
        assert_eq!(err.code(), E_CONFIG_INVALID);
        assert!(err.to_string().starts_with(E_CONFIG_INVALID));
    }

    #[test]
    fn code_returns_matching_const_for_zone_cycle() {
        let err = DormantError::ZoneCycle {
            zone: "living-room".into(),
        };
        assert_eq!(err.code(), E_ZONE_CYCLE);
        assert!(err.to_string().starts_with(E_ZONE_CYCLE));
    }

    #[test]
    fn code_returns_matching_const_for_blank_failed() {
        let failure = CmdFailure {
            controller: "kwin-dpms".into(),
            error: format!("{E_DISPLAY_IO}: timeout"),
        };
        let err = DormantError::BlankFailed(failure);
        assert_eq!(err.code(), E_BLANK_FAILED);
        assert!(err.to_string().starts_with(E_BLANK_FAILED));
    }

    #[test]
    fn code_returns_matching_const_for_sensor_io() {
        let err = DormantError::SensorIo {
            source_id: "usb-ld2410".into(),
            detail: "device disconnected".into(),
        };
        assert_eq!(err.code(), E_SENSOR_IO);
        assert!(err.to_string().starts_with(E_SENSOR_IO));
    }

    #[test]
    fn error_message_starts_with_code() {
        let err = DormantError::ConfigUnknownKey {
            key_path: "sensors.0.type".into(),
        };
        assert!(err.to_string().starts_with(E_CONFIG_UNKNOWN_KEY));

        let err = DormantError::WakeFailed(CmdFailure {
            controller: "ddcci".into(),
            error: format!("{E_DISPLAY_IO}: no monitor found"),
        });
        assert!(err.to_string().starts_with(E_WAKE_FAILED));
    }
}
