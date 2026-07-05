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
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use dormant_core::rules::{ControlMsg, StateSnapshot};
use tokio::sync::oneshot;

use crate::WebState;
use crate::assets;
use crate::error::WebError;
use crate::routes::{command, config, doctor, events};
use crate::security::security_guard;

/// Duration the `/api/state` handler waits for a snapshot reply before
/// returning 504.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the axum [`Router`] on the given state, mounting all HTTP routes
/// behind the Host/Origin security guard.  API routes are nested under
/// `/api`; all other paths are served by the SPA fallback (embedded
/// `webui/dist`).
pub(crate) fn build_router(state: WebState) -> Router {
    let api = Router::new()
        .route("/state", get(get_state))
        .route("/config", get(config::get_config))
        .route("/blank", post(command::post_blank))
        .route("/wake", post(command::post_wake))
        .route("/pause", post(command::post_pause))
        .route("/resume", post(command::post_resume))
        .route("/reload", post(command::post_reload))
        .route("/doctor", post(doctor::post_doctor))
        .route("/events", get(events::ws_events))
        // API miss → 404, never the SPA fallback.
        .fallback(api_not_found)
        .with_state(state.clone());

    Router::new()
        .nest("/api", api)
        .fallback(assets::spa_fallback)
        // Security guard on ALL routes, including the SPA fallback.
        .layer(from_fn_with_state(state.clone(), security_guard))
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

/// API fallback — return 404 for unmatched `/api/*` paths so they are
/// never served the SPA `index.html`.
async fn api_not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found")
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
