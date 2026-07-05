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
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, event) = match self {
            WebError::EngineUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "engine_unavailable"),
            WebError::SnapshotTimeout => (StatusCode::GATEWAY_TIMEOUT, "snapshot_timeout"),
            WebError::SnapshotCancelled => {
                (StatusCode::INTERNAL_SERVER_ERROR, "snapshot_cancelled")
            }
        };
        let body = serde_json::json!({ "error": event });
        (status, axum::Json(body)).into_response()
    }
}
