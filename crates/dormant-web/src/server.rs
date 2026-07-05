//! Axum HTTP server — router + listener lifecycle.
//!
//! Builds a [`Router`] on the caller-supplied [`WebState`] (see
//! [`crate::WebState`]), binds a TCP listener, and serves with graceful
//! shutdown wired to the [`CancellationToken`] in the state.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use dormant_core::rules::{ControlMsg, StateSnapshot};
use tokio::sync::oneshot;

use crate::WebState;
use crate::error::WebError;
use crate::routes::{command, config, doctor, events};
use crate::security::security_guard;

/// Duration the `/api/state` handler waits for a snapshot reply before
/// returning 504.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the axum [`Router`] on the given state, mounting all HTTP routes
/// behind the Host/Origin security guard.
pub(crate) fn build_router(state: WebState) -> Router {
    let router = Router::new()
        .route("/api/state", get(get_state))
        .route("/api/config", get(config::get_config))
        .route("/api/blank", post(command::post_blank))
        .route("/api/wake", post(command::post_wake))
        .route("/api/pause", post(command::post_pause))
        .route("/api/resume", post(command::post_resume))
        .route("/api/reload", post(command::post_reload))
        .route("/api/doctor", post(doctor::post_doctor))
        .route("/api/events", get(events::ws_events))
        .with_state(state.clone());

    // Security guard on ALL routes.
    router.layer(from_fn_with_state(state, security_guard))
}

/// Bind, report the resolved address via `addr_tx`, and serve until the
/// [`CancellationToken`] fires.  Called from the spawned server task.
pub(crate) async fn serve_and_report(
    bind: SocketAddr,
    state: WebState,
    addr_tx: oneshot::Sender<SocketAddr>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    // The receiver may have already been dropped (e.g. if spawn returned
    // early due to a timeout) — ignore the send error.
    let _ = addr_tx.send(local_addr);

    let cancel = state.inner.cancel.clone();
    let router = build_router(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await?;

    Ok(())
}

/// `GET /api/state` — send a [`ControlMsg::Snapshot`] through the
/// control channel, await the reply, and return it as JSON.
async fn get_state(State(state): State<WebState>) -> Result<Json<StateSnapshot>, WebError> {
    let (tx, rx) = oneshot::channel();

    state
        .inner
        .ctl_tx
        .send(ControlMsg::Snapshot(tx))
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    let snapshot = tokio::time::timeout(SNAPSHOT_TIMEOUT, rx)
        .await
        .map_err(|_| WebError::SnapshotTimeout)?
        .map_err(|_| WebError::SnapshotCancelled)?;

    Ok(Json(snapshot))
}
