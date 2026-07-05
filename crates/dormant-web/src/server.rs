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
use axum::routing::get;
use dormant_core::rules::{ControlMsg, StateSnapshot};
use tokio::sync::oneshot;

use crate::WebState;
use crate::error::WebError;

/// Duration the `/api/state` handler waits for a snapshot reply before
/// returning 504.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the axum [`Router`] on the given state.
pub(crate) fn build_router(state: WebState) -> Router {
    Router::new()
        .route("/api/state", get(get_state))
        .with_state(state)
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
