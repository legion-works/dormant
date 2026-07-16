//! Read-only status for long-running web-initiated safety operations.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use serde::Serialize;

use crate::WebState;

/// Browser-visible state of WebState-owned single-flight guards.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OperationsStatus {
    /// Configured display ids with a web exercise awaiting engine completion.
    pub exercise_in_flight: Vec<String>,
    /// Whether a global web emergency wake is awaiting engine completion.
    pub emergency_wake_in_flight: bool,
}

/// `GET /api/operations` — report authoritative web-operation guard state.
pub(crate) async fn get_operations(State(state): State<WebState>) -> impl IntoResponse {
    let mut exercise_in_flight = state
        .inner
        .exercise_in_flight
        .lock()
        .await
        .iter()
        .map(|display| display.0.clone())
        .collect::<Vec<_>>();
    exercise_in_flight.sort();
    let emergency_wake_in_flight = state.inner.emergency_wake_lock.try_lock().is_err();

    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(OperationsStatus {
            exercise_in_flight,
            emergency_wake_in_flight,
        }),
    )
}
