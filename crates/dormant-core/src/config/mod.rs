//! Configuration schema, defaults, loading, and validation for dormant.
//!
//! ## Module layout
//!
//! - [`schema`] — structs that mirror the TOML file shape (serde-driven).
//! - [`defaults`] — every tunable default as a `pub const`.
//! - [`validate()`] — cross-reference checks (zone cycles, credential presence,
//!   unsupported modes, etc.).
//!
//! ## Public API
//!
//! ```ignore
//! let (cfg, warnings) = config::load_config(path, Strictness::Strict)?;
//! let creds = config::load_credentials(creds_path)?;
//! let errors = config::validate(&cfg, &capabilities, &creds);
//! ```

pub mod defaults;
pub mod schema;
pub mod validate;

pub use schema::{
    Config, Credentials, DaemonConfig, DisplayConfig, IdleSource, IdleTimeUnit, MqttCredential,
    RuleConfig, SensorConfig, SensorKind, Strictness, ValidationError, Warning, ZoneConfig,
};
pub use validate::validate;

use std::path::Path;

use crate::error::DormantError;

/// Load a TOML configuration file, applying strict or lenient unknown-key
/// handling.
///
/// # Errors
///
/// - I/O errors from reading the file.
/// - TOML syntax errors.
/// - [`DormantError::ConfigInvalid`] if `config_version` ≠ 1.
/// - [`DormantError::ConfigUnknownKey`] in [`Strictness::Strict`] mode when an
///   unrecognized key is found.
pub fn load_config(
    path: &Path,
    strict: Strictness,
) -> Result<(Config, Vec<Warning>), DormantError> {
    let raw = std::fs::read_to_string(path).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("cannot read config file '{}': {e}", path.display()),
    })?;

    let value: toml::Value = toml::from_str(&raw).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("TOML syntax error: {e}"),
    })?;

    // Walk the TOML tree to discover unknown keys.
    let unknown_keys = validate::collect_unknown_keys(&value);

    let warnings: Vec<Warning> = match strict {
        Strictness::Strict => {
            if let Some(first) = unknown_keys.first() {
                return Err(DormantError::ConfigUnknownKey {
                    key_path: first.key_path.clone(),
                });
            }
            Vec::new()
        }
        Strictness::Warn => unknown_keys
            .into_iter()
            .map(|ve| Warning {
                key_path: ve.key_path,
                message: format!("unknown configuration key: {}", ve.detail),
            })
            .collect(),
    };

    // Deserialize WITHOUT deny_unknown_fields — we already handled that above.
    let cfg: Config = toml::from_str(&raw).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("configuration error: {e}"),
    })?;

    // Config version check.
    if cfg.config_version != 1 {
        return Err(DormantError::ConfigInvalid {
            detail: format!(
                "unsupported config_version {} (this version of dormant expects 1)",
                cfg.config_version,
            ),
        });
    }

    // ── Exactly-one-of blank_mode / ladder (R12 symmetric rule) ─────────────
    for (display_id, dc) in &cfg.displays {
        let has_blank = dc.blank_mode.is_some();
        let has_ladder = !dc.ladder.is_empty();
        let has_degraded = dc.degraded_mode.is_some();

        if has_blank && has_ladder {
            return Err(DormantError::ConfigInvalid {
                detail: format!(
                    "display '{display_id}' set both blank_mode and ladder — set exactly one"
                ),
            });
        }
        if !has_blank && !has_ladder {
            return Err(DormantError::ConfigInvalid {
                detail: format!("display '{display_id}' needs blank_mode or ladder"),
            });
        }
        if has_degraded && has_ladder {
            return Err(DormantError::ConfigInvalid {
                detail: format!(
                    "display '{display_id}' set degraded_mode alongside ladder — \
                     degraded_mode is only valid with blank_mode"
                ),
            });
        }
    }

    Ok((cfg, warnings))
}

/// Load credentials from a TOML file.
///
/// On Unix, the file must have mode `0o600` — anything else returns
/// [`DormantError::CredsPerms`].  If the file does not exist, returns an empty
/// default [`Credentials`].
///
/// # Errors
///
/// - [`DormantError::CredsPerms`] if Unix file permissions are too permissive.
/// - I/O errors for reads on an existing file.
/// - TOML syntax errors.
pub fn load_credentials(path: &Path) -> Result<Credentials, DormantError> {
    if !path.exists() {
        return Ok(Credentials::default());
    }

    // Permissions check — Unix only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|e| DormantError::ConfigInvalid {
            detail: format!("cannot stat credentials file '{}': {e}", path.display()),
        })?;
        let mode = meta.permissions().mode();
        // Require exact 0o600 (owner read+write, group/other nothing).
        if mode & 0o777 != 0o600 {
            return Err(DormantError::CredsPerms {
                path: path.display().to_string(),
            });
        }
    }

    let raw = std::fs::read_to_string(path).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("cannot read credentials file '{}': {e}", path.display()),
    })?;

    let creds: Credentials = toml::from_str(&raw).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("credentials TOML error: {e}"),
    })?;

    Ok(creds)
}
