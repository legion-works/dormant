//! Token-free HTTP forwarding for active wear-sampling consent flows.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dormant_core::ipc_proto::{IpcRequest, IpcResponse, WearSamplingStatus};

use crate::error::WebError;
use crate::{WebState, request_daemon_ipc};

fn status(response: IpcResponse) -> Result<WearSamplingStatus, WebError> {
    response
        .wear_sampling
        .ok_or(WebError::CoordinationUnavailable)
}

/// `POST /api/wear/sampling/enable` — start consent without waiting for its portal window.
pub(crate) async fn post_enable(
    State(state): State<WebState>,
) -> Result<(StatusCode, Json<WearSamplingStatus>), WebError> {
    let Ok(guard) = std::sync::Arc::clone(&state.inner.wear_sampling_lock).try_lock_owned() else {
        return Ok((
            StatusCode::CONFLICT,
            Json(WearSamplingStatus::Error(
                "wear_sampling_in_progress".to_owned(),
            )),
        ));
    };
    tokio::spawn(async move {
        let _guard = guard;
        let _ = request_daemon_ipc(&state, IpcRequest::WearSamplingEnable).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(WearSamplingStatus::AwaitingConsent),
    ))
}

/// `GET /api/wear/sampling` — poll the daemon-owned consent status.
pub(crate) async fn get_status(
    State(state): State<WebState>,
) -> Result<Json<WearSamplingStatus>, WebError> {
    let response = request_daemon_ipc(&state, IpcRequest::WearSamplingStatus).await?;
    Ok(Json(status(response)?))
}

/// `POST /api/wear/sampling/disable` — close sampling, optionally forgetting consent.
pub(crate) async fn post_disable(
    State(state): State<WebState>,
    Json(request): Json<DisableRequest>,
) -> Result<Json<WearSamplingStatus>, WebError> {
    let response = request_daemon_ipc(
        &state,
        IpcRequest::WearSamplingDisable {
            forget: request.forget,
        },
    )
    .await?;
    Ok(Json(status(response)?))
}

#[derive(serde::Deserialize)]
pub(crate) struct DisableRequest {
    pub(crate) forget: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_status_is_the_only_serialized_shape() {
        let body = serde_json::to_value(WearSamplingStatus::Error(
            "wear_sampling_needs_consent".to_owned(),
        ))
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "status": "error",
                "reason": "wear_sampling_needs_consent",
            })
        );
        assert!(body.get("token").is_none());
        assert!(body.get("record").is_none());
        assert!(body.get("persistent_id").is_none());
    }

    #[test]
    fn status_extracts_only_wear_sampling_ipc_payload() {
        let response = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        assert_eq!(status(response).unwrap(), WearSamplingStatus::Granted);
    }
}
