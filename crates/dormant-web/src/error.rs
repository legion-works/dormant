//! HTTP error types for the dormant web API.
//!
//! Maps internal failures (channel closed, oneshot cancelled, timeout) into
//! axum [`StatusCode`] responses.  Every variant carries a grep-stable literal
//! event name for structured logging.
//!
//! ## Body-size limit
//!
//! The `POST /api/config/apply` handler is wrapped in
//! [`axum::extract::DefaultBodyLimit::max`]`(64 * 1024)` (set in
//! [`crate::server::build_router`]).  Bodies larger than 64 KiB are
//! rejected by axum with a 413 `Content-Length Required` / `Payload Too
//! Large` before the handler runs — this satisfies the spec's
//! `BodyTooLarge` requirement without a [`WebError`] variant.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Serializable validation error for the apply response body.
///
/// Mirrors [`crate::routes::config::SerializableValidationError`] but
/// is owned by this module to avoid a circular dependency.
#[derive(serde::Serialize, Debug)]
pub(crate) struct SerializableValidationError {
    what: String,
    detail: String,
}

impl From<&dormant_core::config::ValidationError> for SerializableValidationError {
    fn from(e: &dormant_core::config::ValidationError) -> Self {
        Self {
            what: e.what.clone(),
            detail: e.detail.clone(),
        }
    }
}

/// Errors the web layer can surface to an HTTP caller.
#[derive(Debug)]
pub(crate) enum WebError {
    /// The engine control channel is closed (daemon shutting down).
    EngineUnavailable,
    /// The engine did not reply within the snapshot window.
    SnapshotTimeout,
    /// The snapshot oneshot was cancelled before a reply arrived.
    SnapshotCancelled,
    /// Unknown display name in a blank/wake command.
    UnknownDisplay(String),
    /// Config reload trigger channel is closed.
    ReloadUnavailable,
    /// Config file could not be read for the raw view (future: legacy compat).
    #[allow(dead_code)]
    ConfigReadError(String),
    /// The doctor service panicked or is unavailable (future: health-aware).
    #[allow(dead_code)]
    DoctorUnavailable,
    /// Invalid request body (missing fields, wrong shape).  (future:
    /// stricter validation on command bodies.)
    #[allow(dead_code)]
    BadRequest(String),
    /// The on-disk config fingerprint does not match the one sent by the
    /// client — the config was modified between the GET and the apply.
    FingerprintMismatch,
    /// Daemon-style validation failed on the patched config.
    ValidationFailed(Vec<SerializableValidationError>),
    /// A patch path intersects a redacted (secret) TOML key.
    RedactedPathTargeted(String),
    /// A patch path is not in the known-config-path tree.
    PatchPathDenied(String),
    /// An entity id or index referenced by a patch does not exist in the
    /// current document.
    EntityUnknown(String),
    /// A patch's JSON value could not be converted to TOML.
    PatchValueRejected(String),
    /// The request exceeds the maximum allowed patch count (256).
    PatchCapExceeded(u32),
    /// `CreateEntity` targeted an id that already exists in the collection.
    EntityExists(String),
    /// `POST /api/pair/samsung` was called while
    /// `daemon.pairing_enabled = false`.
    PairFeatureDisabled,
    /// A pairing attempt is already in flight (`pair_lock` `try_lock`
    /// failed).
    PairInProgress,
    /// `GET /api/pair/samsung/{id}` referenced an id with no matching
    /// entry (never existed, or already swept as an expired terminal
    /// entry — see `routes::pair::sweep_expired`).
    PairNotFound,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, event, detail) = match self {
            WebError::EngineUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "engine_unavailable", None)
            }
            WebError::SnapshotTimeout => (StatusCode::GATEWAY_TIMEOUT, "snapshot_timeout", None),
            WebError::SnapshotCancelled => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot_cancelled",
                None,
            ),
            WebError::UnknownDisplay(name) => (
                StatusCode::NOT_FOUND,
                "unknown_display",
                Some(format!("unknown display '{name}'")),
            ),
            WebError::ReloadUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "reload_unavailable", None)
            }
            WebError::ConfigReadError(detail) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_read_error",
                Some(detail),
            ),
            WebError::DoctorUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "doctor_unavailable", None)
            }
            WebError::BadRequest(detail) => (StatusCode::BAD_REQUEST, "bad_request", Some(detail)),

            // ── Config-apply variants ─────────────────────────────────────────
            WebError::FingerprintMismatch => {
                let body = serde_json::json!({ "error": "config changed on disk" });
                return (StatusCode::CONFLICT, axum::Json(body)).into_response();
            }
            WebError::ValidationFailed(errors) => {
                let serialized: Vec<&SerializableValidationError> = errors.iter().collect();
                let body = serde_json::json!({ "errors": serialized });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }
            WebError::RedactedPathTargeted(path) => {
                let body = serde_json::json!({ "errors": [{
                    "what": "redacted_path",
                    "detail": format!("patch targets redacted path: {path}")
                }] });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }
            WebError::PatchPathDenied(path) => {
                let body = serde_json::json!({ "errors": [{
                    "what": "path_denied",
                    "detail": path
                }] });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }
            WebError::EntityUnknown(detail) => {
                let body = serde_json::json!({ "errors": [{
                    "what": "entity_unknown",
                    "detail": detail
                }] });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }
            WebError::PatchValueRejected(detail) => {
                let body = serde_json::json!({ "errors": [{
                    "what": "value_rejected",
                    "detail": detail
                }] });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }
            WebError::PatchCapExceeded(count) => {
                let body = serde_json::json!({ "errors": [{
                    "what": "patch_cap_exceeded",
                    "detail": format!("max 256 patches; received {count}")
                }] });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }
            WebError::EntityExists(detail) => {
                let body = serde_json::json!({ "errors": [{
                    "what": "entity_exists",
                    "detail": detail
                }] });
                return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response();
            }

            // ── Pairing-wizard variants (Task 5) ───────────────────────────────
            WebError::PairFeatureDisabled => (StatusCode::FORBIDDEN, "feature_disabled", None),
            WebError::PairInProgress => (StatusCode::CONFLICT, "pairing_in_progress", None),
            WebError::PairNotFound => (StatusCode::NOT_FOUND, "pair_not_found", None),
        };
        let mut body = serde_json::json!({ "error": event });
        if let Some(d) = detail {
            body["detail"] = serde_json::Value::String(d);
        }
        (status, axum::Json(body)).into_response()
    }
}
