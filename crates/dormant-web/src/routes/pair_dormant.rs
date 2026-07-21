//! Instance pairing routes backed exclusively by the daemon IPC service.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use dormant_core::ipc_proto::{
    CoordinationPairOpenResponse, CoordinationPairStatus, CoordinationPeers, IpcRequest,
    IpcResponse,
};
use serde::{Deserialize, Serialize};

use crate::error::WebError;
use crate::state::WebState;

/// Request body for opening a local responder window.
#[derive(Debug, Deserialize)]
pub(crate) struct InstancePairRequest {
    /// Local display name included in the discovery announcement.
    pub(crate) display_name: String,
}

/// Explicit initiator confirmation request.
#[derive(Debug, Deserialize)]
pub(crate) struct InstancePairJoinRequest {
    pub(crate) display_name: String,
    pub(crate) instance_id: String,
    pub(crate) code: String,
}

/// Status DTO that deliberately excludes the one-time code.
#[derive(Debug, Serialize)]
pub(crate) struct InstancePairStatus {
    state: String,
    detail: Option<String>,
}

fn enabled(state: &WebState) -> Result<(), WebError> {
    if state.inner.config_rx.borrow().coordination.enabled {
        Ok(())
    } else {
        Err(WebError::CoordinationDisabled)
    }
}

async fn request(state: &WebState, request: IpcRequest) -> Result<IpcResponse, WebError> {
    let socket = dormant_core::paths::resolve_socket_path(
        state.inner.config_rx.borrow().daemon.socket_path.as_deref(),
    );
    tokio::task::spawn_blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)
            .map_err(|_| WebError::CoordinationUnavailable)?;
        let line =
            serde_json::to_string(&request).map_err(|_| WebError::CoordinationUnavailable)?;
        writeln!(stream, "{line}").map_err(|_| WebError::CoordinationUnavailable)?;
        stream
            .flush()
            .map_err(|_| WebError::CoordinationUnavailable)?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|_| WebError::CoordinationUnavailable)?;
        serde_json::from_str(&line).map_err(|_| WebError::CoordinationUnavailable)
    })
    .await
    .map_err(|_| WebError::CoordinationUnavailable)?
}

fn status(response: IpcResponse) -> Result<InstancePairStatus, WebError> {
    let CoordinationPairStatus {
        state,
        peer_instance_id,
        ..
    } = response
        .coordination_pair
        .ok_or(WebError::CoordinationUnavailable)?;
    Ok(InstancePairStatus {
        state,
        detail: peer_instance_id,
    })
}

/// Open a local pairing window and return its one-time code exactly once.
pub(crate) async fn post_pair_instance(
    State(state): State<WebState>,
    Json(body): Json<InstancePairRequest>,
) -> Result<(StatusCode, Json<CoordinationPairOpenResponse>), WebError> {
    enabled(&state)?;
    let Ok(guard) = Arc::clone(&state.inner.pair_lock).try_lock_owned() else {
        return Err(WebError::PairInProgress);
    };
    let response = request(
        &state,
        IpcRequest::CoordinationPairOpen {
            display_name: body.display_name,
        },
    )
    .await?;
    drop(guard);
    let opened = response
        .coordination_pair_open
        .ok_or(WebError::CoordinationUnavailable)?;
    Ok((StatusCode::ACCEPTED, Json(opened)))
}

/// Return non-secret lifecycle state for a local responder window.
pub(crate) async fn get_pair_instance(
    State(state): State<WebState>,
    Path(pair_id): Path<String>,
) -> Result<Json<InstancePairStatus>, WebError> {
    enabled(&state)?;
    status(request(&state, IpcRequest::CoordinationPairStatus { pair_id }).await?).map(Json)
}

/// Submit an operator-confirmed code to a selected discovered instance.
pub(crate) async fn post_join_pair_instance(
    State(state): State<WebState>,
    Json(body): Json<InstancePairJoinRequest>,
) -> Result<(StatusCode, Json<InstancePairStatus>), WebError> {
    enabled(&state)?;
    let response = request(
        &state,
        IpcRequest::CoordinationPairJoin {
            display_name: body.display_name,
            instance_id: body.instance_id,
            code: body.code,
        },
    )
    .await?;
    if !response.ok {
        return Err(WebError::CoordinationUnavailable);
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(InstancePairStatus {
            state: "pairing".into(),
            detail: None,
        }),
    ))
}

/// Cancel a local responder window.
pub(crate) async fn post_cancel_pair_instance(
    State(state): State<WebState>,
    Path(pair_id): Path<String>,
) -> Result<(StatusCode, Json<InstancePairStatus>), WebError> {
    enabled(&state)?;
    let status = status(request(&state, IpcRequest::CoordinationPairCancel { pair_id }).await?)?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

/// Return the read-only public inventory used by the pairing UI.
pub(crate) async fn get_pair_instance_peers(
    State(state): State<WebState>,
) -> Result<Json<CoordinationPeers>, WebError> {
    enabled(&state)?;
    let response = request(&state, IpcRequest::CoordinationPeersList).await?;
    response
        .coordination_peers
        .map(Json)
        .ok_or(WebError::CoordinationUnavailable)
}
