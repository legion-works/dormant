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
pub use validate::{is_known_config_path, validate};

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

/// Atomically upsert a Samsung TV pairing token into `credentials.toml`.
///
/// Adds or replaces the entry `[samsung]."<host>" = "<token>"` while preserving
/// every other table, key, and comment in the file. If `creds_path` does not
/// exist, a new file is created with only the samsung entry.
///
/// The write is atomic (temp file in the same directory + rename) and the file
/// is created with mode `0o600` on Unix. The temp file is cleaned up on error.
///
/// # Errors
///
/// Returns [`DormantError::ConfigInvalid`] on I/O or parse errors.
pub fn upsert_samsung_token(
    creds_path: &Path,
    host: &str,
    token: &str,
) -> Result<(), DormantError> {
    use std::io::Write as _;

    let mut doc: toml_edit::DocumentMut = if creds_path.exists() {
        let raw = std::fs::read_to_string(creds_path).map_err(|e| DormantError::ConfigInvalid {
            detail: format!(
                "cannot read credentials file '{}': {e}",
                creds_path.display()
            ),
        })?;
        raw.parse().map_err(|e| DormantError::ConfigInvalid {
            detail: format!("credentials TOML error: {e}"),
        })?
    } else {
        toml_edit::DocumentMut::new()
    };

    // Ensure the [samsung] table is an explicit table (not inline), so
    // dotted-IP keys are quoted correctly and the format matches what
    // load_credentials expects.
    if doc.get("samsung").is_none() {
        let mut tbl = toml_edit::Table::new();
        tbl.set_implicit(false);
        doc["samsung"] = toml_edit::Item::Table(tbl);
    }
    doc["samsung"][host] = toml_edit::value(token);

    let serialized = doc.to_string();

    // Write to a sibling temp file in the same directory for atomic rename.
    let dir = creds_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let tmp_path = dir.join(".credentials.toml.tmp");

    let write_result = (|| -> Result<(), DormantError> {
        // Create with 0o600 before writing secret bytes.
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| DormantError::ConfigInvalid {
                    detail: format!("cannot create temp credentials file: {e}"),
                })?
        };

        #[cfg(not(unix))]
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| DormantError::ConfigInvalid {
                detail: format!("cannot create temp credentials file: {e}"),
            })?;

        f.write_all(serialized.as_bytes())
            .map_err(|e| DormantError::ConfigInvalid {
                detail: format!("cannot write temp credentials file: {e}"),
            })?;
        f.flush().map_err(|e| DormantError::ConfigInvalid {
            detail: format!("cannot flush temp credentials file: {e}"),
        })?;
        f.sync_all().map_err(|e| DormantError::ConfigInvalid {
            detail: format!("cannot sync temp credentials file: {e}"),
        })?;

        Ok(())
    })();

    match write_result {
        Ok(()) => {
            std::fs::rename(&tmp_path, creds_path).map_err(|e| DormantError::ConfigInvalid {
                detail: format!("cannot rename temp to credentials file: {e}"),
            })?;
            Ok(())
        }
        Err(e) => {
            // Best-effort cleanup of the temp file.
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_creds(path: &Path) {
        let content = r#"
# credentials for dormant — KEEP THIS FILE 0600

[mqtt."mqtt://x:1883"]
username = "sensor1"
password = "secret"

# existing samsung tokens
[samsung]
"1.2.3.4" = "old"
"#;
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn upsert_samsung_token_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        seed_creds(&creds_path);

        upsert_samsung_token(&creds_path, "192.0.2.7", "example-token-1234").unwrap();

        let raw = std::fs::read_to_string(&creds_path).unwrap();
        // (a) mqtt entry survives
        assert!(raw.contains("mqtt.\"mqtt://x:1883\""), "mqtt entry removed");
        // (b) comment survives
        assert!(
            raw.contains("# credentials for dormant"),
            "header comment removed"
        );
        assert!(raw.contains("# existing samsung tokens"), "comment removed");
        // (c) old samsung host survives
        assert!(raw.contains("\"1.2.3.4\""), "old samsung host removed");
        assert!(raw.contains("\"old\""), "old samsung token removed");
        // (d) new host present and quoted
        assert!(
            raw.contains("\"192.0.2.7\""),
            "new host not quoted — dotted IP would parse as nested table"
        );
        assert!(raw.contains("\"example-token-1234\""), "new token missing");
        // (e) re-parsing via real Credentials deser yields both samsung hosts
        let creds: Credentials = toml::from_str(&raw).unwrap();
        assert_eq!(
            creds.samsung.get("1.2.3.4").map(String::as_str),
            Some("old")
        );
        assert_eq!(
            creds.samsung.get("192.0.2.7").map(String::as_str),
            Some("example-token-1234")
        );
        // (f) file mode is 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&creds_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "file mode is not 0600");
        }
    }

    #[test]
    fn upsert_samsung_token_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");

        upsert_samsung_token(&creds_path, "10.0.0.1", "abc123").unwrap();

        let raw = std::fs::read_to_string(&creds_path).unwrap();
        assert!(raw.contains("[samsung]"), "missing [samsung] section");
        assert!(raw.contains("\"10.0.0.1\""), "host not quoted");
        assert!(raw.contains("\"abc123\""), "token missing");

        let creds: Credentials = toml::from_str(&raw).unwrap();
        assert_eq!(
            creds.samsung.get("10.0.0.1").map(String::as_str),
            Some("abc123")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&creds_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "file mode is not 0600");
        }
    }
}
