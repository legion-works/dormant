//! `POST /api/config/apply` — atomic config patching with fingerprint
//! conflict detection, daemon-identical validation, and atomic
//! backup+rename.
//!
//! ## Design invariants
//!
//! - All work happens inside [`tokio::sync::Mutex`]`<()>` — concurrent
//!   applies serialise, so only one writer touches the config file at a
//!   time.
//! - Comments and formatting in the original TOML are preserved via
//!   [`toml_edit::DocumentMut`].
//! - Validation is byte-identical to what the daemon runs at startup
//!   ([`load_config`], [`load_credentials`], [`validate`]), so a patch
//!   that passes this endpoint will survive a daemon restart.
//! - The current config is backed up to a `0o700` backups directory
//!   before the temp file replaces it via atomic rename.
//! - The write-temp→fsync→rename sequence ensures no half-written file
//!   survives a crash.

use std::path::{Path, PathBuf};

use crate::WebState;
use crate::config_patch::{Patch, PatchError, apply_patches, check_patches};
use crate::error::{SerializableValidationError, WebError};
use crate::routes::config::redact_config_secrets;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use dormant_core::config::{Strictness, load_config, load_credentials, validate};
use dormant_core::observation::{ContentRevision, ReloadSource};
use dormant_core::reload::ReloadOutcome;
use dormant_displays::registry::capabilities;

// ── Request / response types ──────────────────────────────────────────────

/// Request body for `POST /api/config/apply`.
#[derive(Deserialize, Debug)]
pub(crate) struct ApplyRequest {
    /// Lowercase hex SHA-256 of the on-disk config bytes as returned by
    /// `GET /api/config`.  Mismatch → 409.
    pub fingerprint: String,
    /// Ordered list of patches to apply.
    pub patches: Vec<Patch>,
}

/// Response for `POST /api/config/apply`.
#[derive(Serialize, Debug)]
pub(crate) struct ApplyResponse {
    pub applied: bool,
    /// Reload outcome: `"reloaded"`, `"rejected"`, `"superseded"`, or
    /// `"pending"` (timeout/lagged).
    pub reload: String,
    /// Human-readable detail when `reload == "rejected"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ── Patch cap ─────────────────────────────────────────────────────────────

/// Maximum number of patches accepted in a single apply request.
const PATCH_CAP: usize = 256;

// ── Backup helpers ────────────────────────────────────────────────────────

/// Max backups to retain in the backups directory.
const MAX_BACKUPS: usize = 5;

/// Build a backup filename: `config.toml.<rfc3339-nanos>.<rand4>`.
///
/// Uses [`humantime::format_rfc3339_nanos`] for a standard timestamp;
/// colons are replaced with dashes for filesystem safety.
/// This is a pure function so it can be unit-tested.
fn backup_filename(now: std::time::SystemTime, rand: &str) -> String {
    let ts = humantime::format_rfc3339_nanos(now).to_string();
    // Replace colons with dashes for filesystem-safe filenames.
    let safe_ts = ts.replace(':', "-");
    format!("config.toml.{safe_ts}.{rand}")
}

/// Generate a random 4-character hex string.
fn rand4() -> String {
    let val: u16 = rand::random();
    format!("{val:04x}")
}

// ── Handler ────────────────────────────────────────────────────────────────

// This handler is a single linear, explicitly step-numbered pipeline (see
// the step comments below and the module doc comment) — splitting it up
// would scatter one atomic operation across several functions for no
// readability gain. It sat right at clippy's 100-line pedantic threshold
// before this change; the `entity_created`/`entity_deleted` audit-log call
// (spec §11 invariant 8, §14) tips it to 101.
#[allow(clippy::too_many_lines)]
pub(crate) async fn post_apply(
    State(state): State<WebState>,
    Json(body): Json<ApplyRequest>,
) -> Result<Json<ApplyResponse>, WebError> {
    // Cap check — reject large patch sets before taking the lock.
    if body.patches.len() > PATCH_CAP {
        return Err(WebError::PatchCapExceeded(
            body.patches.len().try_into().unwrap_or(u32::MAX),
        ));
    }

    // Serialise: only one apply at a time.
    let _lock = state.inner.apply_lock.lock().await;

    // ── Step 1: re-read file bytes, fingerprint check ──────────────────
    let raw_bytes = std::fs::read(&state.inner.config_path)
        .map_err(|e| WebError::ConfigReadError(format!("cannot read config: {e}")))?;

    let actual_fingerprint = format!("{:x}", Sha256::digest(&raw_bytes));
    if actual_fingerprint != body.fingerprint {
        return Err(WebError::FingerprintMismatch);
    }

    // ── Step 2: double-parse the SAME bytes ────────────────────────────
    let raw = String::from_utf8_lossy(&raw_bytes).into_owned();

    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| WebError::ConfigReadError(format!("toml parse error: {e}")))?;

    let mut cfg: dormant_core::config::schema::Config = toml::from_str(&raw)
        .map_err(|e| WebError::ConfigReadError(format!("config deserialize error: {e}")))?;

    // ── Step 2b: entity_crud_enabled gate (spec §2) ─────────────────────
    // The switch gates the CRUD *verbs* (CreateEntity/DeleteEntity), not
    // ordinary edits — Set/Remove ride the existing 5-stage pipeline
    // unchanged (spec §11 invariant 2, §0: "the existing 5-stage
    // pipeline, UNCHANGED"). Read from `cfg`, the SAME just-re-read,
    // fingerprint-matched bytes `check_patches` below validates against
    // (not the live `config_rx` watch, which could momentarily lag a
    // concurrent on-disk edit) — this is the strictest, race-free read of
    // "current" available in this handler.
    let wants_entity_crud = body
        .patches
        .iter()
        .any(|p| matches!(p, Patch::CreateEntity { .. } | Patch::DeleteEntity { .. }));
    if wants_entity_crud && !cfg.daemon.entity_crud_enabled {
        return Err(WebError::EntityCrudFeatureDisabled);
    }

    // ── Step 3: redact for redacted-path checking ──────────────────────
    let redacted = redact_config_secrets(&mut cfg);

    // ── Step 4: check patches ──────────────────────────────────────────
    if let Err(e) = check_patches(&body.patches, &doc, &redacted) {
        return Err(patch_error_to_web(e));
    }

    // ── Step 5: apply patches ──────────────────────────────────────────
    if let Err(e) = apply_patches(&mut doc, &body.patches) {
        return Err(patch_error_to_web(e));
    }

    let patched_toml = doc.to_string();

    // ── Step 6: write temp, validate ───────────────────────────────────
    let config_dir = state
        .inner
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."));

    let temp_path = write_temp(config_dir, &patched_toml)?;

    // Daemon-identical validation on the temp file — explicit match,
    // no `?` (no From<DormantError> for WebError).
    let (cfg, _warnings) = match load_config(&temp_path, Strictness::Strict) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(WebError::ConfigReadError(e.to_string()));
        }
    };

    let creds = match load_credentials(&state.inner.creds_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(WebError::ConfigReadError(format!("credentials error: {e}")));
        }
    };

    let errors = validate(&cfg, &capabilities(), &creds);
    if !errors.is_empty() {
        let _ = std::fs::remove_file(&temp_path);
        let serialized: Vec<SerializableValidationError> = errors
            .iter()
            .map(SerializableValidationError::from)
            .collect();
        return Err(WebError::ValidationFailed(serialized));
    }

    // ── Step 7: backup current config ─────────────────────────────────
    backup_current(&state.inner.config_path, config_dir)?;

    // ── Step 8: fsync temp, atomic rename, fsync dir ───────────────────
    sync_file(&temp_path)?;
    std::fs::rename(&temp_path, &state.inner.config_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        WebError::ConfigReadError(format!("rename failed: {e}"))
    })?;
    sync_dir(config_dir)?;
    // Step 8b: audit-log, now that steps 4-8 have ALL succeeded (spec §11
    // invariant 8, §14; see `log_entity_audit_events`'s doc comment).
    log_entity_audit_events(&body.patches);

    // ── Step 9: register the receipt waiter before enqueueing the reload ──
    let written_revision = ContentRevision::from_bytes(patched_toml.as_bytes());
    let (_, receipt_rx) = state
        .inner
        .reload_requester
        .request(ReloadSource::WebApply)
        .await
        .ok_or(WebError::ReloadUnavailable)?;
    let timeout = state.inner.reload_timeout;

    let (reload, detail) = match tokio::time::timeout(timeout, receipt_rx).await {
        Ok(Ok(receipt)) => {
            if receipt.requested_revision.config == written_revision {
                match receipt.outcome {
                    ReloadOutcome::Reloaded => (String::from("reloaded"), None),
                    ReloadOutcome::Rejected(detail) => (String::from("rejected"), Some(detail)),
                }
            } else {
                (String::from("superseded"), None)
            }
        }
        Ok(Err(_closed)) => (String::from("pending"), None),
        Err(_elapsed) => {
            // Timeout — daemon hasn't responded yet or reload is stalled.
            (String::from("pending"), None)
        }
    };

    Ok(Json(ApplyResponse {
        applied: true,
        reload,
        detail,
    }))
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// Map a [`PatchError`] to the corresponding [`WebError`] variant.
fn patch_error_to_web(e: PatchError) -> WebError {
    match e {
        PatchError::PathDenied(msg) => WebError::PatchPathDenied(msg),
        PatchError::RedactedPath(msg) => WebError::RedactedPathTargeted(msg),
        PatchError::EntityUnknown(msg) => WebError::EntityUnknown(msg),
        PatchError::ValueRejected(msg) => WebError::PatchValueRejected(msg),
        PatchError::EntityExists(msg) => WebError::EntityExists(msg),
    }
}

/// Emit `entity_created`/`entity_deleted` audit events for every
/// [`Patch::CreateEntity`]/[`Patch::DeleteEntity`] in `patches` (spec §11
/// invariant 8, §14 — grep-stable literal event names).
///
/// Callers MUST only invoke this once the patch set has been fully applied
/// AND durably written to disk — never on a rejected patch. Deliberately
/// logs `collection`+`id` ONLY: an entity's `value` can carry secrets
/// (broker URLs, hostnames, tokens) and must never reach a log line (same
/// concern [`redact_config_secrets`] handles for reads).
fn log_entity_audit_events(patches: &[Patch]) {
    for patch in patches {
        match patch {
            Patch::CreateEntity { collection, id, .. } => {
                tracing::info!(event = "entity_created", collection = %collection, id = %id);
            }
            Patch::DeleteEntity { collection, id } => {
                tracing::info!(event = "entity_deleted", collection = %collection, id = %id);
            }
            Patch::Set { .. } | Patch::Remove { .. } => {}
        }
    }
}

/// Write `content` to `config.toml.tmp.<rand4>` in `dir`.  Returns the
/// temp file path.
fn write_temp(dir: &Path, content: &str) -> Result<PathBuf, WebError> {
    let tmp_name = format!("config.toml.tmp.{}", rand4());
    let tmp_path = dir.join(&tmp_name);
    std::fs::write(&tmp_path, content)
        .map_err(|e| WebError::ConfigReadError(format!("cannot write temp config: {e}")))?;
    Ok(tmp_path)
}

/// fsync a regular file (data + metadata).
#[cfg(unix)]
fn sync_file(path: &Path) -> Result<(), WebError> {
    let file = std::fs::File::open(path)
        .map_err(|e| WebError::ConfigReadError(format!("cannot open for fsync: {e}")))?;
    file.sync_all()
        .map_err(|e| WebError::ConfigReadError(format!("fsync failed: {e}")))
}

#[cfg(not(unix))]
fn sync_file(path: &Path) -> Result<(), WebError> {
    let file = std::fs::File::open(path)
        .map_err(|e| WebError::ConfigReadError(format!("cannot open for fsync: {e}")))?;
    file.sync_all()
        .map_err(|e| WebError::ConfigReadError(format!("fsync failed: {e}")))
}

/// fsync a directory to ensure the rename is durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), WebError> {
    let file = std::fs::File::open(dir)
        .map_err(|e| WebError::ConfigReadError(format!("cannot open dir for fsync: {e}")))?;
    file.sync_all()
        .map_err(|e| WebError::ConfigReadError(format!("dir fsync failed: {e}")))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), WebError> {
    // Non-Unix: rename durability is up to the filesystem.
    Ok(())
}

/// Backup the current config to `<dir>/backups/config.toml.<ts>.<rand>`.
/// Prunes the backup dir to keep at most [`MAX_BACKUPS`] entries.
fn backup_current(config_path: &Path, config_dir: &Path) -> Result<(), WebError> {
    let backups_dir = config_dir.join("backups");

    // Create with mode 0o700.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&backups_dir)
            .map_err(|e| WebError::ConfigReadError(format!("cannot create backups dir: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&backups_dir)
            .map_err(|e| WebError::ConfigReadError(format!("cannot create backups dir: {e}")))?;
    }

    // Generate a unique backup filename (retry on collision).
    let backup_path = loop {
        let name = backup_filename(std::time::SystemTime::now(), &rand4());
        let candidate = backups_dir.join(&name);
        if !candidate.exists() {
            break candidate;
        }
    };

    std::fs::copy(config_path, &backup_path)
        .map_err(|e| WebError::ConfigReadError(format!("backup failed: {e}")))?;

    // Prune: keep at most MAX_BACKUPS newest by filename sort.
    prune_backups(&backups_dir)?;

    Ok(())
}

/// Keep at most [`MAX_BACKUPS`] newest backups (sorted by filename,
/// which encodes rfc3339 timestamps).  Extra files are deleted.
fn prune_backups(backups_dir: &Path) -> Result<(), WebError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(backups_dir)
        .map_err(|e| WebError::ConfigReadError(format!("cannot read backups dir: {e}")))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("config.toml."))
        })
        .collect();

    if entries.len() <= MAX_BACKUPS {
        return Ok(());
    }

    // Sort by filename descending (newest first) — rfc3339 timestamps
    // sort lexicographically.
    entries.sort_by(|a, b| {
        b.file_name()
            .and_then(|n| n.to_str())
            .cmp(&a.file_name().and_then(|n| n.to_str()))
    });

    // Delete all but the first MAX_BACKUPS.
    for path in entries.iter().skip(MAX_BACKUPS) {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WebError;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use dormant_core::config::schema::{
        Config, DaemonConfig, RuleConfig, SensorConfig, ZoneConfig,
    };
    use dormant_core::reload::ReloadOutcome;
    use indexmap::IndexMap;
    use serde_json::json;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::broadcast;

    use crate::test_support::{start_capturing, take_captured};

    // ── Test helpers ────────────────────────────────────────────────────────

    /// Write `content` to `dir/config.toml` and a minimal creds file.
    fn write_config(dir: &std::path::Path, content: &str) {
        std::fs::write(dir.join("config.toml"), content).unwrap();
        let creds = dir.join("config.creds.toml");
        std::fs::write(&creds, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&creds).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&creds, perms).unwrap();
        }
    }

    /// Build a [`WebState`] suitable for testing the apply handler.
    fn test_state(config_dir: &std::path::Path, config: Config, bind_port: u16) -> WebState {
        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, mut reload_trigger_rx) =
            tokio::sync::mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(config));
        let creds = Arc::new(dormant_core::config::schema::Credentials::default());
        let (creds_tx, creds_rx) = tokio::sync::watch::channel(creds);
        let cancel = tokio_util::sync::CancellationToken::new();

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let config_path = config_dir.join("config.toml");
        let creds_path = config_dir.join("config.creds.toml");
        let receipt_config_path = config_path.clone();
        let receipt_creds_path = creds_path.clone();
        tokio::spawn(async move {
            while let Some(request) = reload_trigger_rx.recv().await {
                let config = std::fs::read(&receipt_config_path).unwrap_or_default();
                let credentials = std::fs::read(&receipt_creds_path).map_or_else(
                    |_| dormant_core::observation::ContentRevision::missing(),
                    |bytes| dormant_core::observation::ContentRevision::from_bytes(&bytes),
                );
                let revision = dormant_core::observation::RuntimeRevision {
                    config: dormant_core::observation::ContentRevision::from_bytes(&config),
                    credentials,
                };
                if let Some(waiter) = request.receipt_tx {
                    let _ = waiter.send(dormant_core::observation::ReloadReceipt {
                        request_ids: vec![request.request_id],
                        sources: vec![request.source],
                        requested_revision: revision.clone(),
                        applied_revision: revision,
                        generation: dormant_core::observation::GenerationId(0),
                        outcome: ReloadOutcome::Reloaded,
                        coalesced: false,
                    });
                }
            }
        });

        WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx,
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path,
                creds_path,
                doctor,
                wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bind_port),
                cancel,
                reload_timeout: Duration::from_secs(10),
            },
        ))
    }

    /// Like [`test_state`] but returns the [`broadcast::Sender`]`<`[`ReloadOutcome`]`>`
    /// so the test can control the reload outcome the apply handler awaits.
    fn test_state_with_reload(
        config_dir: &std::path::Path,
        config: Config,
        bind_port: u16,
        reload_timeout: Duration,
    ) -> (WebState, broadcast::Sender<ReloadOutcome>) {
        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, mut reload_trigger_rx) =
            tokio::sync::mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(config));
        let creds = Arc::new(dormant_core::config::schema::Credentials::default());
        let (creds_tx, creds_rx) = tokio::sync::watch::channel(creds);
        let cancel = tokio_util::sync::CancellationToken::new();

        // Senders that must stay alive for the channel to remain open.
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let config_path = config_dir.join("config.toml");
        let creds_path = config_dir.join("config.creds.toml");
        let receipt_config_path = config_path.clone();
        let receipt_creds_path = creds_path.clone();
        let mut outcomes = reload_tx.subscribe();
        tokio::spawn(async move {
            while let Some(request) = reload_trigger_rx.recv().await {
                let outcome = outcomes.recv().await.unwrap_or(ReloadOutcome::Reloaded);
                let config = std::fs::read(&receipt_config_path).unwrap_or_default();
                let credentials = std::fs::read(&receipt_creds_path).map_or_else(
                    |_| dormant_core::observation::ContentRevision::missing(),
                    |bytes| dormant_core::observation::ContentRevision::from_bytes(&bytes),
                );
                let revision = dormant_core::observation::RuntimeRevision {
                    config: dormant_core::observation::ContentRevision::from_bytes(&config),
                    credentials,
                };
                if let Some(waiter) = request.receipt_tx {
                    let _ = waiter.send(dormant_core::observation::ReloadReceipt {
                        request_ids: vec![request.request_id],
                        sources: vec![request.source],
                        requested_revision: revision.clone(),
                        applied_revision: revision,
                        generation: dormant_core::observation::GenerationId(0),
                        outcome,
                        coalesced: false,
                    });
                }
            }
        });

        let state = WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx,
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path,
                creds_path,
                doctor,
                wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bind_port),
                cancel,
                reload_timeout,
            },
        ));

        (state, reload_tx)
    }

    /// A minimal valid config for apply tests.
    fn minimal_config() -> Config {
        Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
        }
    }

    /// A config with a rule that can be patched.
    fn config_with_rule() -> (Config, &'static str) {
        let mut rules = IndexMap::new();
        rules.insert(
            "myrule".into(),
            RuleConfig {
                zone: "myzone".into(),
                displays: vec![],
                grace_period: std::time::Duration::from_secs(5),
                min_blank_time: std::time::Duration::from_secs(30),
                min_wake_time: std::time::Duration::from_secs(30),
                inhibitors: vec![],
                activity_idle_threshold: std::time::Duration::from_secs(300),
                activity_poll_interval: std::time::Duration::from_secs(30),
                wake_retries: 3,
                wake_retry_backoff: std::time::Duration::from_secs(2),
                wake_retry_interval: std::time::Duration::from_secs(2),
            },
        );
        let mut zones = IndexMap::new();
        zones.insert(
            "myzone".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec![],
                quorum: None,
                threshold: None,
                weights: IndexMap::default(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        let cfg = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones,
            displays: IndexMap::default(),
            rules,
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
        };
        (cfg, "myrule")
    }

    /// Get the fingerprint for the current on-disk config.
    fn get_fingerprint(state: &WebState) -> String {
        let bytes = std::fs::read(&state.inner.config_path).unwrap();
        format!("{:x}", Sha256::digest(&bytes))
    }

    // ── backup_filename tests ───────────────────────────────────────────────

    #[test]
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn backup_filename_isomorphic_with_known_time() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(42);
        let name = backup_filename(now, "abcd");
        assert!(
            name.starts_with("config.toml.1970-01-01T00-00-42"),
            "{name}"
        );
        assert!(name.ends_with(".abcd"), "{name}");
    }

    #[test]
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn backup_filename_handles_2026() {
        // 2026-07-07T12:34:56.789Z
        let secs: u64 = 20_641 * 86_400 + 12 * 3600 + 34 * 60 + 56;
        let nsecs: u32 = 789_000_000;
        let dur = std::time::Duration::new(secs, nsecs);
        let now = std::time::UNIX_EPOCH + dur;
        let name = backup_filename(now, "f001");
        assert!(
            name.starts_with("config.toml.2026-07-07T12-34-56.789"),
            "{name}"
        );
        assert!(name.ends_with(".f001"), "{name}");
    }

    #[test]
    fn backup_filename_collision_retry_unit() {
        // Verify the filename changes with different rand values.
        let now = std::time::SystemTime::now();
        let a = backup_filename(now, "aaaa");
        let b = backup_filename(now, "bbbb");
        assert_ne!(a, b);
    }

    // ── Happy-path test ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn happy_path_200_file_changed_comments_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let content = "# top comment\nconfig_version = 1\n[daemon]\n";
        write_config(dir.path(), content);

        let (cfg, _rule_id) = config_with_rule();
        let state = test_state(dir.path(), cfg, 8080);
        let fingerprint = get_fingerprint(&state);

        // Patch: add a comment-like value that demonstrates TOML preservation.
        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let result = post_apply(State(state.clone()), axum::Json(req)).await;
        assert!(
            result.is_ok(),
            "happy path should succeed: {:?}",
            result.err()
        );

        // Verify the file was changed.
        let new_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        let new_content = String::from_utf8_lossy(&new_bytes);
        // The comment must be preserved.
        assert!(new_content.contains("# top comment"), "comment preserved");
        // The patch must be applied.
        assert!(new_content.contains("web_bind"), "patch applied");

        // Backup must exist in 0o700 dir.
        let backups_dir = dir.path().join("backups");
        assert!(backups_dir.exists(), "backups dir created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&backups_dir).unwrap();
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o700,
                "backups dir mode 0o700"
            );
        }

        let backup_files: Vec<_> = std::fs::read_dir(&backups_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(backup_files.len(), 1, "one backup created");
        let backup_name = backup_files[0].file_name();
        assert!(
            backup_name.to_str().unwrap().starts_with("config.toml."),
            "backup filename: {backup_name:?}"
        );
    }

    // ── Fingerprint mismatch → 409 ──────────────────────────────────────────

    #[tokio::test]
    async fn wrong_fingerprint_409_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);

        let original_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();

        let req = ApplyRequest {
            fingerprint: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            patches: vec![],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // File must be unchanged.
        let after_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        assert_eq!(original_bytes, after_bytes, "file unchanged after 409");
    }

    // ── Validation failure → 422 ────────────────────────────────────────────

    #[tokio::test]
    async fn validation_fail_wake_retry_interval_zero_returns_422() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"
config_version = 1

[zones.myzone]
mode = "any"
members = []

[rules.myrule]
zone = "myzone"
displays = []
wake_retry_interval = "5s"
"#;
        write_config(dir.path(), content);

        let (cfg, rule_id) = config_with_rule();
        let state = test_state(dir.path(), cfg, 8080);
        let fingerprint = get_fingerprint(&state);

        // Patch wake_retry_interval to "0s" → validation must fail.
        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["rules".into(), rule_id.into(), "wake_retry_interval".into()],
                value: serde_json::Value::String("0s".into()),
            }],
        };

        let original_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // File unchanged.
        let after_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "file unchanged after validation fail"
        );

        // No temp left.
        let temps: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("config.toml.tmp."))
            })
            .collect();
        assert!(temps.is_empty(), "no temp files left");
    }

    // ── Redacted path → 422 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn redacted_path_patch_returns_422() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"
config_version = 1
[sensors.desk]
type = "mqtt"
broker_url = "tcp://u:p@h:1883"
topic = "test"
field = "/val"
"#;
        write_config(dir.path(), content);

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "desk".into(),
            SensorConfig::Mqtt(dormant_core::config::schema::MqttSensorCfg {
                broker_url: "tcp://u:p@h:1883".into(),
                topic: "test".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: dormant_core::config::schema::SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        let cfg = Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
        };
        let state = test_state(dir.path(), cfg, 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["sensors".into(), "desk".into(), "broker_url".into()],
                value: serde_json::Value::String("tcp://new@host:1883".into()),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    // ── Entity unknown → 422 ────────────────────────────────────────────────

    #[tokio::test]
    async fn entity_unknown_returns_422() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["rules".into(), "nonexistent".into(), "grace_period".into()],
                value: serde_json::Value::String("10s".into()),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    // ── Create missing structural tables ───────────────────────────────────

    #[tokio::test]
    async fn set_daemon_log_level_on_minimal_config_creates_table() {
        let dir = tempfile::tempdir().unwrap();
        // Config containing ONLY config_version — no [daemon] section.
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "log_level".into()],
                value: serde_json::Value::String("debug".into()),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        assert!(
            result.is_ok(),
            "set daemon.log_level on minimal config should create [daemon] table: {:?}",
            result.err()
        );

        let new_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        let new_content = String::from_utf8_lossy(&new_bytes);
        assert!(
            new_content.contains("[daemon]") || new_content.contains("log_level"),
            "file should contain daemon section or log_level: {new_content}"
        );
    }

    #[tokio::test]
    async fn set_on_missing_entity_still_denied() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["sensors".into(), "ghost".into(), "topic".into()],
                value: serde_json::Value::String("test".into()),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "patching missing entity should be 422"
        );
    }

    // ── Patch cap exceeded → 422 ────────────────────────────────────────────

    #[tokio::test]
    async fn patch_cap_257_returns_422() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);

        // Build 257 empty patches — the cap check happens before any
        // validation, so the fingerprint can be wrong.
        let patches = (0..257)
            .map(|_| Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            })
            .collect();

        let req = ApplyRequest {
            fingerprint: "any".into(),
            patches,
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    // ── Missing Content-Type → 415 (existing guard) ─────────────────────────

    #[tokio::test]
    async fn missing_content_type_returns_415() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::util::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let router = crate::server::build_router(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config/apply")
            .header("Host", "127.0.0.1:8080")
            .header("Origin", "http://127.0.0.1:8080")
            .body(Body::from(r#"{"fingerprint":"x","patches":[]}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // ── Origin security — wrong-port loopback → 403 ─────────────────────────

    #[tokio::test]
    async fn apply_wrong_port_origin_returns_403() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::util::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let router = crate::server::build_router(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config/apply")
            .header("Host", "127.0.0.1:8080")
            .header("Content-Type", "application/json")
            .header("Origin", "http://127.0.0.1:9999")
            .body(Body::from(r#"{"fingerprint":"x","patches":[]}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "wrong-port Origin should be 403"
        );
    }

    // ── Origin security — exact match passes ────────────────────────────────

    #[tokio::test]
    async fn apply_exact_origin_passes() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::util::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n[daemon]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);
        let router = crate::server::build_router(state);

        let body = serde_json::json!({
            "fingerprint": fingerprint,
            "patches": []
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config/apply")
            .header("Host", "127.0.0.1:8080")
            .header("Content-Type", "application/json")
            .header("Origin", "http://127.0.0.1:8080")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "exact Origin should pass");
    }

    // ── Origin security — missing Origin → 403 ──────────────────────────────

    #[tokio::test]
    async fn apply_missing_origin_returns_403() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::util::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let router = crate::server::build_router(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config/apply")
            .header("Host", "127.0.0.1:8080")
            .header("Content-Type", "application/json")
            // No Origin header.
            .body(Body::from(r#"{"fingerprint":"x","patches":[]}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "missing Origin should be 403 on apply"
        );
    }

    // ── Body size >64KiB → 413 ──────────────────────────────────────────────

    #[tokio::test]
    async fn body_too_large_returns_413() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::util::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let router = crate::server::build_router(state);

        let big_body = "x".repeat(70_000);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config/apply")
            .header("Host", "127.0.0.1:8080")
            .header("Content-Type", "application/json")
            .header("Origin", "http://127.0.0.1:8080")
            .body(Body::from(big_body))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "body > 64KiB should be 413"
        );
    }

    // ── Backup rotation ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn backup_rotation_keeps_5_newest() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n[daemon]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);

        for i in 0..7 {
            // Re-fetch fingerprint for each apply (409s don't produce backups).
            let fp = get_fingerprint(&state);
            let req = ApplyRequest {
                fingerprint: fp,
                patches: vec![Patch::Set {
                    path: vec!["daemon".into(), "web_bind".into()],
                    value: serde_json::Value::String("127.0.0.1".into()),
                }],
            };
            let result = post_apply(State(state.clone()), axum::Json(req)).await;
            assert!(result.is_ok(), "apply {} failed: {:?}", i, result.err());
        }

        let backups_dir = dir.path().join("backups");
        let backup_files: Vec<_> = std::fs::read_dir(&backups_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("config.toml."))
            })
            .collect();

        assert_eq!(backup_files.len(), 5, "exactly 5 backups after 7 applies");
    }

    // ── Concurrent applies ──────────────────────────────────────────────────

    #[tokio::test]
    async fn two_concurrent_applies_one_200_one_409() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n[daemon]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let s1 = state.clone();
        let s2 = state.clone();
        let req1 = axum::Json(ApplyRequest {
            fingerprint: req.fingerprint.clone(),
            patches: req.patches.clone(),
        });
        let req2 = axum::Json(req);

        let (r1, r2) = tokio::join!(post_apply(State(s1), req1), post_apply(State(s2), req2),);

        // Exactly one should succeed, one should be 409 (fingerprint mismatch
        // because the first one already changed the file).
        let status1 = match &r1 {
            Ok(_) => StatusCode::OK,
            Err(e) => {
                // Clone the error to get the status; WebError doesn't impl Clone,
                // so we match.
                match e {
                    WebError::FingerprintMismatch => StatusCode::CONFLICT,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                }
            }
        };
        let status2 = match &r2 {
            Ok(_) => StatusCode::OK,
            Err(e) => match e {
                WebError::FingerprintMismatch => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
        };

        let statuses = [status1, status2];
        assert!(
            statuses.contains(&StatusCode::OK) && statuses.contains(&StatusCode::CONFLICT),
            "expected one 200 and one 409, got {status1:?} and {status2:?}"
        );
    }

    /// Deterministic test: hold the apply lock, spawn an apply, verify it
    /// blocks, release the lock, verify it completes.
    #[tokio::test]
    async fn apply_blocks_until_lock_released() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n[daemon]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        // Hold the lock so the spawned apply cannot proceed.
        let lock = state.inner.apply_lock.lock().await;

        let spawn_state = state.clone();
        let spawn_req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let handle =
            tokio::spawn(
                async move { post_apply(State(spawn_state), axum::Json(spawn_req)).await },
            );

        // The spawned apply must NOT complete while we hold the lock.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "spawned apply should block while lock is held"
        );

        // Release the lock.
        drop(lock);

        // Now the spawned apply should complete.
        let result = handle.await.expect("spawned task should not panic");
        assert!(
            result.is_ok(),
            "apply should succeed after lock released: {:?}",
            result.err()
        );
    }

    // ── Stale temp cleanup test ─────────────────────────────────────────────

    #[tokio::test]
    async fn stale_temp_cleanup_keeps_fresh_files() {
        let dir = tempfile::tempdir().unwrap();

        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "config_version = 1\n").unwrap();

        let fresh_temp = dir.path().join("config.toml.tmp.fresh");
        std::fs::write(&fresh_temp, "fresh").unwrap();

        // Run prune — should not panic or remove fresh files.
        crate::prune_stale_temps(&config_path);

        assert!(fresh_temp.exists(), "fresh temp should survive");
    }

    // ── Reload-outcome tests ──────────────────────────────────────────────

    /// Reloaded outcome + fingerprint matches → `"reloaded"`.
    #[tokio::test]
    async fn reload_sync_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let (state, reload_tx) = test_state_with_reload(
            dir.path(),
            minimal_config(),
            8080,
            Duration::from_millis(500),
        );
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let handle = tokio::spawn(async move { post_apply(State(state), axum::Json(req)).await });

        // Allow the handler to subscribe to the reload bus.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = reload_tx.send(ReloadOutcome::Reloaded);

        let result = handle.await.unwrap().unwrap();
        assert!(result.applied);
        assert_eq!(result.reload, "reloaded");
        assert!(result.detail.is_none());
    }

    /// Rejected outcome + fingerprint matches → `"rejected"` with detail.
    #[tokio::test]
    async fn reload_sync_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let (state, reload_tx) = test_state_with_reload(
            dir.path(),
            minimal_config(),
            8080,
            Duration::from_millis(500),
        );
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let handle = tokio::spawn(async move { post_apply(State(state), axum::Json(req)).await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = reload_tx.send(ReloadOutcome::Rejected("invalid zone config".into()));

        let result = handle.await.unwrap().unwrap();
        assert!(result.applied);
        assert_eq!(result.reload, "rejected");
        assert_eq!(result.detail.as_deref(), Some("invalid zone config"));
    }

    /// Outcome arrives but on-disk fingerprint changed (another writer) →
    /// `"superseded"`.
    #[tokio::test]
    async fn reload_sync_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let (state, reload_tx) = test_state_with_reload(
            dir.path(),
            minimal_config(),
            8080,
            Duration::from_millis(500),
        );
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let config_path = state.inner.config_path.clone();
        let handle = tokio::spawn(async move { post_apply(State(state), axum::Json(req)).await });

        // Let the handler write and rename the file, then subscribe.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Another writer overwrites the file AFTER ours was renamed.
        std::fs::write(&config_path, "config_version = 1\n# superseded\n").unwrap();

        let _ = reload_tx.send(ReloadOutcome::Reloaded);

        let result = handle.await.unwrap().unwrap();
        assert!(result.applied);
        assert_eq!(result.reload, "superseded");
    }

    /// No outcome within the short timeout window → `"pending"`.
    #[tokio::test]
    async fn reload_sync_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let (state, _reload_tx) = test_state_with_reload(
            dir.path(),
            minimal_config(),
            8080,
            Duration::from_millis(50),
        );
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await.unwrap();
        assert!(result.applied);
        assert_eq!(result.reload, "pending");
    }

    /// Receipt delivery survives unrelated reload-bus overflow.
    #[tokio::test]
    async fn reload_sync_lagged() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n";
        write_config(dir.path(), content);

        let (state, reload_tx) = test_state_with_reload(
            dir.path(),
            minimal_config(),
            8080,
            Duration::from_millis(500),
        );
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "web_bind".into()],
                value: serde_json::Value::String("127.0.0.1".into()),
            }],
        };

        // Overflow the compatibility broadcast before the request is enqueued.
        for i in 0..17 {
            let _ = reload_tx.send(ReloadOutcome::Reloaded);
            // The first 16 fill the ring buffer; the 17th evicts the oldest.
            // When the handler arrives, it sees Lagged.
            let _ = i;
        }

        let handle = tokio::spawn(async move { post_apply(State(state), axum::Json(req)).await });

        // The request's one-shot receipt is independent of the overflowed bus.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.applied);
        assert_eq!(result.reload, "reloaded");
    }

    // ── Task 2 Step 5: CreateEntity/DeleteEntity apply integration ──────────
    // (the daemon-identical-validate net — config_apply.rs is the ONLY place
    // that runs the real dormant_core::config::validate() on a patched doc)

    #[tokio::test]
    async fn create_display_unknown_controllers_422_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let content = "config_version = 1\n[daemon]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);
        let original_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                collection: "displays".into(),
                id: "tv".into(),
                value: json!({
                    "controllers": ["nonexistent-controller"],
                    "blank_mode": "power_off",
                }),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown controller must fail the daemon-identical validate"
        );

        let after_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "real config file must be untouched when validate rejects the create"
        );
    }

    #[tokio::test]
    async fn delete_zone_referenced_by_rule_422_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"
config_version = 1

[zones.myzone]
mode = "any"
members = []

[rules.myrule]
zone = "myzone"
displays = []
"#;
        write_config(dir.path(), content);

        let (cfg, _rule_id) = config_with_rule();
        let state = test_state(dir.path(), cfg, 8080);
        let fingerprint = get_fingerprint(&state);
        let original_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::DeleteEntity {
                collection: "zones".into(),
                id: "myzone".into(),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "deleting a zone still referenced by a rule must fail the reference-integrity net"
        );

        let after_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "real config file must be untouched when the delete orphans a reference"
        );
    }

    #[tokio::test]
    async fn create_then_delete_entity_round_trip_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let content = "# top comment\nconfig_version = 1\n\n# sensors section\n[sensors]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config(), 8080);

        // ── Step 1: create a sensor ─────────────────────────────────────
        let fingerprint = get_fingerprint(&state);
        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                collection: "sensors".into(),
                id: "newsensor".into(),
                value: json!({
                    "type": "mqtt",
                    "broker_url": "mqtt://localhost",
                    "topic": "presence/new",
                }),
            }],
        };
        let result = post_apply(State(state.clone()), axum::Json(req)).await;
        assert!(result.is_ok(), "create should succeed: {:?}", result.err());

        let after_create = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(
            after_create.contains("# top comment"),
            "comment preserved after create"
        );
        assert!(
            after_create.contains("# sensors section"),
            "comment preserved after create"
        );
        assert!(
            after_create.contains("[sensors.newsensor]"),
            "created entity present:\n{after_create}"
        );

        let backups_dir = dir.path().join("backups");
        let backup_count_after_create = std::fs::read_dir(&backups_dir).unwrap().count();
        assert_eq!(
            backup_count_after_create, 1,
            "create backed up the pre-create file"
        );

        // ── Step 2: delete the same sensor ──────────────────────────────
        let fingerprint2 = get_fingerprint(&state);
        let req2 = ApplyRequest {
            fingerprint: fingerprint2,
            patches: vec![Patch::DeleteEntity {
                collection: "sensors".into(),
                id: "newsensor".into(),
            }],
        };
        let result2 = post_apply(State(state), axum::Json(req2)).await;
        assert!(
            result2.is_ok(),
            "delete should succeed: {:?}",
            result2.err()
        );

        let after_delete = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(
            after_delete.contains("# top comment"),
            "comment preserved after delete"
        );
        assert!(
            after_delete.contains("# sensors section"),
            "comment preserved after delete"
        );
        assert!(
            !after_delete.contains("newsensor"),
            "deleted entity gone:\n{after_delete}"
        );

        let backup_count_after_delete = std::fs::read_dir(&backups_dir).unwrap().count();
        assert_eq!(
            backup_count_after_delete, 2,
            "delete backed up the post-create file (atomic rename on both legs)"
        );
    }

    #[tokio::test]
    async fn created_sensor_renders_as_explicit_table_not_inline() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                collection: "sensors".into(),
                id: "golden".into(),
                value: json!({
                    "type": "mqtt",
                    "broker_url": "mqtt://localhost",
                    "topic": "presence/golden",
                }),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        assert!(result.is_ok(), "create should succeed: {:?}", result.err());

        let after = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        // Golden assertion: an explicit `[sensors.golden]` section, never a
        // one-line inline table (`golden = { ... }`).
        assert!(
            after.contains("[sensors.golden]"),
            "expected explicit [sensors.golden] section, got:\n{after}"
        );
        assert!(
            !after.contains("golden = {"),
            "must not render as an inline table, got:\n{after}"
        );
        assert!(after.contains(r#"type = "mqtt""#));
        assert!(after.contains(r#"broker_url = "mqtt://localhost""#));
    }

    // ── entity_crud_enabled server-side enforcement (cross-task Must) ───────
    // T1 shipped `daemon.entity_crud_enabled` (default true) with zero
    // server-side gate: `check_create_entity`/`check_delete_entity` never
    // consulted it, so a client could POST create_entity/delete_entity with
    // the flag off and succeed. Spec §2: "When a switch is false, the route
    // returns 403 `feature_disabled` ... the UI hides the affordance but the
    // server is the boundary." `Set`/`Remove` patches are NOT gated by this
    // flag (spec §2 talks about "the route" for the CRUD verbs; §11
    // invariant 2 confirms ordinary `Set`/`Remove` editing is unrelated,
    // pre-existing behaviour this spec does not touch).

    fn minimal_config_crud_disabled() -> Config {
        let mut cfg = minimal_config();
        cfg.daemon.entity_crud_enabled = false;
        cfg
    }

    #[tokio::test]
    async fn create_entity_denied_when_crud_disabled_403_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        // post_apply re-parses `cfg` fresh from THIS on-disk content (not
        // from the in-memory `Config` handed to `test_state`, which only
        // seeds `config_rx`) — the flag must live in the file.
        let content = "config_version = 1\n[daemon]\nentity_crud_enabled = false\n[sensors]\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config_crud_disabled(), 8080);
        let fingerprint = get_fingerprint(&state);
        let original_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                collection: "sensors".into(),
                id: "newsensor".into(),
                value: json!({
                    "type": "mqtt",
                    "broker_url": "mqtt://localhost",
                    "topic": "presence/new",
                }),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "create_entity must be denied 403 when daemon.entity_crud_enabled is false"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "feature_disabled");

        let after_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "real config file must be untouched when entity CRUD is disabled"
        );
    }

    #[tokio::test]
    async fn delete_entity_denied_when_crud_disabled_403_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        // post_apply re-parses `cfg` fresh from THIS on-disk content (not
        // from the in-memory `Config` handed to `test_state`) — the flag
        // must live in the file.
        let content = r#"
config_version = 1

[daemon]
entity_crud_enabled = false

[zones.myzone]
mode = "any"
members = []
"#;
        write_config(dir.path(), content);

        let mut cfg = minimal_config();
        cfg.daemon.entity_crud_enabled = false;
        let mut zones = IndexMap::new();
        zones.insert(
            "myzone".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec![],
                quorum: None,
                threshold: None,
                weights: IndexMap::default(),
                unavailable_policy: dormant_core::zone::UnavailablePolicy::Present,
            },
        );
        cfg.zones = zones;
        let state = test_state(dir.path(), cfg, 8080);
        let fingerprint = get_fingerprint(&state);
        let original_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::DeleteEntity {
                collection: "zones".into(),
                id: "myzone".into(),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        let err = result.unwrap_err();
        let resp = err.into_response();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "delete_entity must be denied 403 when daemon.entity_crud_enabled is false"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "feature_disabled");

        let after_bytes = std::fs::read(dir.path().join("config.toml")).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "real config file must be untouched when entity CRUD is disabled"
        );
    }

    #[tokio::test]
    async fn ordinary_set_patch_unaffected_by_crud_disabled() {
        let dir = tempfile::tempdir().unwrap();
        // entity_crud_enabled = false in the ACTUAL on-disk content (what
        // post_apply re-parses `cfg` from) — a plain Set must still work.
        let content = "config_version = 1\n[daemon]\nentity_crud_enabled = false\n";
        write_config(dir.path(), content);

        let state = test_state(dir.path(), minimal_config_crud_disabled(), 8080);
        let fingerprint = get_fingerprint(&state);

        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::Set {
                path: vec!["daemon".into(), "log_level".into()],
                value: serde_json::Value::String("debug".into()),
            }],
        };

        let result = post_apply(State(state), axum::Json(req)).await;
        assert!(
            result.is_ok(),
            "an ordinary Set patch must still succeed when entity_crud_enabled is false \
             (the flag gates create_entity/delete_entity only, not Set/Remove): {:?}",
            result.err()
        );
    }

    // ── entity_created / entity_deleted audit events (spec §11 invariant 8,
    // §14) ────────────────────────────────────────────────────────────────
    // These literal `event = "entity_created"` / `event = "entity_deleted"`
    // tracing events are grep-stable per spec but, prior to this fix, were
    // never emitted anywhere. The capture layer is shared with
    // `routes::pair`'s tests via `crate::test_support` — see that module's
    // comment for why a per-module `Once`-gated global subscriber would
    // race and silently blind whichever module lost.

    #[tokio::test]
    async fn successful_create_emits_entity_created_not_value() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n[sensors]\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        start_capturing();
        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                collection: "sensors".into(),
                id: "golden".into(),
                value: json!({
                    "type": "mqtt",
                    "broker_url": "mqtt://s3cret-host-must-not-leak",
                    "topic": "presence/golden",
                }),
            }],
        };
        let result = post_apply(State(state), axum::Json(req)).await;
        let events = take_captured();

        assert!(result.is_ok(), "create should succeed: {:?}", result.err());
        let joined = events.join("\n");
        assert!(
            joined.contains("entity_created"),
            "missing entity_created in: {joined}"
        );
        assert!(
            joined.contains("collection=sensors"),
            "missing collection field in: {joined}"
        );
        assert!(
            joined.contains("id=golden"),
            "missing id field in: {joined}"
        );
        assert!(
            !joined.contains("s3cret-host-must-not-leak"),
            "entity value payload must never be logged: {joined}"
        );
        assert!(
            !joined.contains("broker_url"),
            "entity value payload must never be logged: {joined}"
        );
    }

    #[tokio::test]
    async fn successful_delete_emits_entity_deleted() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n[sensors]\n");

        let state = test_state(dir.path(), minimal_config(), 8080);

        // Create first (uncaptured) so there's something to delete.
        let fingerprint = get_fingerprint(&state);
        let create_req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                collection: "sensors".into(),
                id: "todelete".into(),
                value: json!({
                    "type": "mqtt",
                    "broker_url": "mqtt://localhost",
                    "topic": "presence/todelete",
                }),
            }],
        };
        let result = post_apply(State(state.clone()), axum::Json(create_req)).await;
        assert!(result.is_ok(), "create should succeed: {:?}", result.err());

        start_capturing();
        let fingerprint2 = get_fingerprint(&state);
        let delete_req = ApplyRequest {
            fingerprint: fingerprint2,
            patches: vec![Patch::DeleteEntity {
                collection: "sensors".into(),
                id: "todelete".into(),
            }],
        };
        let result2 = post_apply(State(state), axum::Json(delete_req)).await;
        let events = take_captured();

        assert!(
            result2.is_ok(),
            "delete should succeed: {:?}",
            result2.err()
        );
        let joined = events.join("\n");
        assert!(
            joined.contains("entity_deleted"),
            "missing entity_deleted in: {joined}"
        );
        assert!(
            joined.contains("collection=sensors"),
            "missing collection field in: {joined}"
        );
        assert!(
            joined.contains("id=todelete"),
            "missing id field in: {joined}"
        );
    }

    #[tokio::test]
    async fn failed_create_reserved_id_emits_no_entity_created() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "config_version = 1\n[sensors]\n");

        let state = test_state(dir.path(), minimal_config(), 8080);
        let fingerprint = get_fingerprint(&state);

        start_capturing();
        let req = ApplyRequest {
            fingerprint,
            patches: vec![Patch::CreateEntity {
                // "type" is in RESERVED_ENTITY_IDS — rejected by
                // check_patches, BEFORE apply_patches ever runs.
                collection: "sensors".into(),
                id: "type".into(),
                value: json!({
                    "type": "mqtt",
                    "broker_url": "mqtt://localhost",
                    "topic": "presence/x",
                }),
            }],
        };
        let result = post_apply(State(state), axum::Json(req)).await;
        let events = take_captured();

        assert!(
            result.is_err(),
            "create with a reserved id must be rejected"
        );
        let joined = events.join("\n");
        assert!(
            !joined.contains("entity_created"),
            "a rejected create must never emit entity_created: {joined}"
        );
    }
}
