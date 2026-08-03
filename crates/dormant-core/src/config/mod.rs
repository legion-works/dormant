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
    Config, CoordinationConfig, Credentials, DaemonConfig, DisplayConfig, DisplayScope, HookAction,
    HookCommand, HookMqtt, HookSlots, IdleSource, IdleTimeUnit, InputFilterConfig, KeymapConfig,
    MqttCredential, PublishConfig, RuleConfig, SensorConfig, SensorKind, Strictness,
    ValidationError, Warning, ZoneConfig,
};
pub use validate::{
    ClaimValidationContext, STRUCTURAL_RESERVED_NAMES, collect_macos_power_off_warnings,
    is_known_config_path, is_macos_power_off_hazard, validate, validate_with_input_source_readers,
};

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

    load_config_from_str(&raw, strict)
}

/// Parse configuration bytes, applying strict or lenient unknown-key handling.
///
/// # Errors
///
/// Returns [`DormantError::ConfigInvalid`] when the bytes are not valid UTF-8,
/// contain invalid TOML, or specify an unsupported configuration version.
pub fn load_config_from_bytes(
    raw: &[u8],
    strict: Strictness,
) -> Result<(Config, Vec<Warning>), DormantError> {
    let raw = std::str::from_utf8(raw).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("configuration is not valid UTF-8: {e}"),
    })?;

    load_config_from_str(raw, strict)
}

/// Parse a configuration document, applying strict or lenient unknown-key handling.
///
/// # Errors
///
/// - TOML syntax errors.
/// - [`DormantError::ConfigInvalid`] if `config_version` ≠ 1.
/// - [`DormantError::ConfigUnknownKey`] in [`Strictness::Strict`] mode when an
///   unrecognized key is found.
pub fn load_config_from_str(
    raw: &str,
    strict: Strictness,
) -> Result<(Config, Vec<Warning>), DormantError> {
    let value: toml::Value = toml::from_str(raw).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("TOML syntax error: {e}"),
    })?;

    // Walk the TOML tree to discover unknown keys.
    let unknown_keys = validate::collect_unknown_keys(&value);

    let mut warnings: Vec<Warning> = match strict {
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
    let cfg: Config = toml::from_str(raw).map_err(|e| DormantError::ConfigInvalid {
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

    // ── Semantic warnings (issue #126 macOS power-off hazard) ───────────────
    // These are config-time hazard flags that must surface in BOTH
    // strictness modes — unlike unknown-key warnings, they are not the
    // validator's primary concern and the operator can never forget
    // them by setting strictness to "warn". Merged into the same
    // `Warning` vec so CLI validate and web apply both see them.
    //
    // Platform-gated: the USB-C link drop is a macOS-specific
    // phenomenon, so a Linux daemon must not surface a "macOS DDC/CI
    // topology" warning for a hazard that doesn't exist on its host.
    // `cfg!(target_os = "macos")` is a compile-time host check —
    // non-macOS builds compile the collector but always pass `false`,
    // the macOS build compiles the same call site with `true`.
    warnings.extend(validate::collect_macos_power_off_warnings(
        &cfg,
        cfg!(target_os = "macos"),
    ));
    warnings.extend(validate::collect_active_sampling_warnings(&cfg));

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

    load_credentials_from_str(&raw)
}

/// Parse credentials bytes from an already-read document.
///
/// # Errors
///
/// Returns [`DormantError::ConfigInvalid`] when the bytes are not valid UTF-8
/// or contain invalid credentials TOML.
pub fn load_credentials_from_bytes(raw: &[u8]) -> Result<Credentials, DormantError> {
    let raw = std::str::from_utf8(raw).map_err(|e| DormantError::ConfigInvalid {
        detail: format!("credentials are not valid UTF-8: {e}"),
    })?;

    load_credentials_from_str(raw)
}

/// Parse credentials from an already-read TOML document.
///
/// # Errors
///
/// Returns [`DormantError::ConfigInvalid`] for invalid credentials TOML.
pub fn load_credentials_from_str(raw: &str) -> Result<Credentials, DormantError> {
    let creds: Credentials = toml::from_str(raw).map_err(|e| DormantError::ConfigInvalid {
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
/// Two concurrent calls for the SAME `creds_path` (e.g. the CLI pair command
/// and the web pair route writing to the same file) are serialized through a
/// process-wide, per-path `Mutex<()>`, so the read→edit→write window is
/// never interleaved (issue #195). The temp file is opened with
/// `create_new(true)` and (on Unix) `O_NOFOLLOW` so a pre-planted symlink at
/// the temp path cannot capture the credential bytes — the unique temp name
/// makes a pre-existing file unreachable in practice, `O_NOFOLLOW` is the
/// defense in depth if the unique name ever collides.
///
/// # Errors
///
/// Returns [`DormantError::ConfigInvalid`] on I/O or parse errors.
///
/// # Panics
///
/// Panics only if the process-wide per-path `Mutex<()>` is poisoned — that
/// requires a prior thread to have panicked while holding the lock for this
/// `creds_path`. In practice the lock is held for a few filesystem syscalls,
/// so a panic here is a programming error, not a runtime condition.
pub fn upsert_samsung_token(
    creds_path: &Path,
    host: &str,
    token: &str,
) -> Result<(), DormantError> {
    use std::io::Write as _;

    // ── Per-path serialization (issue #195) ──────────────────────────────────
    // Hold the per-path `Mutex<()>` across the read→edit→temp-write→rename
    // window. Without this, two callers see a stale read AND race the temp
    // filename; one token is lost.
    let lock = lock_for(creds_path);
    let _guard = lock.lock().expect("creds lock poisoned");

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
    let tmp_path = unique_temp_path(dir);

    let write_result: Result<(), DormantError> = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // `create_new(true)` refuses to open any pre-existing file (regular,
            // symlink, fifo, anything). `O_NOFOLLOW` is defense in depth: if
            // somehow a symlink slipped in at the temp path (a TOCTOU race
            // between name generation and open), the kernel refuses to follow
            // it, so credential bytes never reach the attacker-controlled
            // target.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
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
        }
        #[cfg(not(unix))]
        {
            // Non-Unix has no portable `O_NOFOLLOW`; the symlink defense here
            // is `create_new(true)` alone. The unique temp name makes a
            // pre-existing file (symlink or otherwise) practically
            // unreachable, and `create_new(true)` is the formal guarantee: if
            // the path exists for any reason, open fails rather than follows.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
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
        }
    })();

    match write_result {
        Ok(()) => {
            std::fs::rename(&tmp_path, creds_path).map_err(|e| DormantError::ConfigInvalid {
                detail: format!("cannot rename temp to credentials file: {e}"),
            })?;
            Ok(())
        }
        Err(e) => {
            // Best-effort cleanup of the caller-owned temp.
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

// ── upsert_samsung_token helpers ─────────────────────────────────────────────

/// Process-global counter disambiguating concurrent `upsert_samsung_token`
/// calls so the unique temp name never repeats within this process.
static UPSERT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-path serialization registry. Two calls for the same `creds_path` MUST
/// hold the per-path `Mutex<()>` for the entire read→edit→temp-write→rename
/// window (issue #195). Distinct paths use distinct `Mutex<()>` instances so
/// unrelated writes don't serialize against each other.
static CREDS_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, std::sync::Arc<std::sync::Mutex<()>>>,
    >,
> = std::sync::OnceLock::new();

fn creds_locks() -> &'static std::sync::Mutex<
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<std::sync::Mutex<()>>>,
> {
    CREDS_LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Return the per-path `Mutex<()>`. The path is normalized so two literal
/// paths that refer to the same physical file (relative vs absolute, symlink
/// vs resolved) share one lock.
fn lock_for(creds_path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    let key = lock_key_for(creds_path);
    let mut map = creds_locks().lock().expect("creds locks map poisoned");
    map.entry(key)
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// Normalize `creds_path` into a stable lock key. Canonicalize the parent so
/// `./credentials.toml` and the absolute path to the same file share one
/// `Mutex<()>`. Fall back to the literal parent if canonicalization fails
/// (the parent doesn't exist yet on a fresh install, for example).
fn lock_key_for(creds_path: &Path) -> std::path::PathBuf {
    let parent = creds_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = creds_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    let canonical_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    canonical_parent.join(file_name)
}

/// Generate a unique sibling temp path in `dir`. Each call yields a fresh
/// name: pid + monotonic clock + process-global sequence. The unique name
/// alone is necessary but not sufficient — the per-path `Mutex<()>` above
/// covers the read/edit window.
fn unique_temp_path(dir: &Path) -> std::path::PathBuf {
    let seq = UPSERT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    dir.join(format!(".credentials.toml.tmp.{pid}.{nanos}.{seq}"))
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

    #[test]
    fn byte_parsers_accept_the_same_minimal_documents_as_path_loaders() {
        let (config, warnings) =
            load_config_from_bytes(b"config_version = 1\n", Strictness::Strict).unwrap();
        assert_eq!(config.config_version, 1);
        assert!(warnings.is_empty());
        assert_eq!(
            load_credentials_from_bytes(b"").unwrap(),
            Credentials::default()
        );
    }

    /// Two concurrent `upsert_samsung_token` calls against the SAME credentials
    /// file with DISTINCT host keys must not lose either token. Issue #195: the
    /// fixed temp filename clobbered one call's write/rename pair, and even
    /// without clobber the read→edit→write window is wide open without
    /// serialization. This test races two threads on a barrier; the final file
    /// MUST contain both `[samsung]` entries.
    #[test]
    fn upsert_samsung_token_concurrent_distinct_hosts_both_persist() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");
        seed_creds(&creds_path);

        // Two distinct hosts, two distinct tokens — both must survive.
        let barrier = Arc::new(Barrier::new(2));
        let path_a = creds_path.clone();
        let path_b = creds_path.clone();
        let barrier_a = Arc::clone(&barrier);
        let barrier_b = Arc::clone(&barrier);

        let host_a = "192.0.2.10";
        let host_b = "192.0.2.11";
        let token_a = "token-alpha-1234";
        let token_b = "token-bravo-5678";

        let t_a = std::thread::spawn(move || {
            barrier_a.wait();
            upsert_samsung_token(&path_a, host_a, token_a)
        });
        let t_b = std::thread::spawn(move || {
            barrier_b.wait();
            upsert_samsung_token(&path_b, host_b, token_b)
        });

        // Both upserts must individually succeed; the bug is data loss, not
        // I/O failure.
        t_a.join().expect("thread a panicked").unwrap();
        t_b.join().expect("thread b panicked").unwrap();

        let raw = std::fs::read_to_string(&creds_path).unwrap();
        let creds: Credentials = toml::from_str(&raw).expect("final creds must parse");
        assert_eq!(
            creds.samsung.get(host_a).map(String::as_str),
            Some(token_a),
            "host A token lost after concurrent upsert; final file:\n{raw}"
        );
        assert_eq!(
            creds.samsung.get(host_b).map(String::as_str),
            Some(token_b),
            "host B token lost after concurrent upsert; final file:\n{raw}"
        );
        // The pre-existing samsung entry (and mqtt/comments) from `seed_creds`
        // must also survive — the race fix must not regress atomicity.
        assert_eq!(
            creds.samsung.get("1.2.3.4").map(String::as_str),
            Some("old"),
            "pre-existing samsung entry lost; final file:\n{raw}"
        );
        assert!(
            creds.mqtt.contains_key("mqtt://x:1883"),
            "pre-existing mqtt entry lost; final file:\n{raw}"
        );
    }

    /// Unix TOCTOU guard for the temp filename. If an attacker pre-plants a
    /// symlink at the temp path the upsert previously used, the old
    /// implementation followed it and wrote credential bytes through the
    /// symlink to the attacker's target. The fix must NEVER open a path that
    /// resolves through a symlink for credential bytes.
    #[cfg(unix)]
    #[test]
    fn upsert_samsung_token_does_not_follow_symlinked_temp_on_unix() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.toml");

        // Sentinel the symlink points to — the old fixed-name code would have
        // written the new credentials to THIS file through the symlink.
        let sentinel = dir.path().join("attacker-target.toml");
        let sentinel_content = "PWNED-BY-SYMLINK\n";
        std::fs::write(&sentinel, sentinel_content).unwrap();

        // Plant a symlink at the historical fixed temp name. The fixed name is
        // the trap — the hardened implementation must pick a different
        // sibling name and never touch this path.
        let symlinked_temp = dir.path().join(".credentials.toml.tmp");
        symlink(&sentinel, &symlinked_temp).expect("plant symlink at fixed temp name");

        // Run the upsert — it must succeed AND leave the sentinel untouched.
        upsert_samsung_token(&creds_path, "192.0.2.20", "fresh-token-9abc").unwrap();

        // Real credentials file got the new host.
        let raw = std::fs::read_to_string(&creds_path).unwrap();
        let creds: Credentials = toml::from_str(&raw).expect("creds must parse");
        assert_eq!(
            creds.samsung.get("192.0.2.20").map(String::as_str),
            Some("fresh-token-9abc"),
            "real credentials file did not receive the new host; contents:\n{raw}"
        );

        // The sentinel MUST be byte-for-byte what we wrote — no credential
        // bytes leaked through the symlink.
        let sentinel_after = std::fs::read_to_string(&sentinel).unwrap();
        assert_eq!(
            sentinel_after,
            sentinel_content,
            "credential bytes leaked through pre-planted symlink at {}\n\
             sentinel now contains:\n{sentinel_after}\n\
             expected:\n{sentinel_content}",
            symlinked_temp.display()
        );
    }
}
