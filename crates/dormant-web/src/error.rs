//! HTTP error types for the dormant web API.
//!
//! Maps internal failures (channel closed, oneshot cancelled, timeout) into
//! axum [`StatusCode`] responses.  Every variant carries a grep-stable literal
//! event name for structured logging.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

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
    /// Config file could not be read for the raw view.
    ConfigReadError(String),
    /// The doctor service panicked or is unavailable (future: health-aware).
    #[allow(dead_code)]
    DoctorUnavailable,
    /// Invalid request body (missing fields, wrong shape).  (future:
    /// stricter validation on command bodies.)
    #[allow(dead_code)]
    BadRequest(String),
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
        };
        let mut body = serde_json::json!({ "error": event });
        if let Some(d) = detail {
            body["detail"] = serde_json::Value::String(d);
        }
        (status, axum::Json(body)).into_response()
    }
}
