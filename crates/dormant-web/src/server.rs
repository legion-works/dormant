//! Axum HTTP server — router + listener lifecycle.
//!
//! Builds a [`Router`] on the caller-supplied [`WebState`] (see
//! [`crate::WebState`]), binds a TCP listener, and serves with graceful
//! shutdown wired to the [`CancellationToken`] in the state.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use dormant_core::rules::{ControlMsg, StateSnapshot};
use tokio::sync::oneshot;

use crate::WebState;
use crate::assets;
use crate::error::WebError;
use crate::routes::{command, config, config_apply, doctor, events};
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
        .route(
            "/config/apply",
            post(config_apply::post_apply).layer(DefaultBodyLimit::max(64 * 1024)),
        )
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

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, header};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use tower::util::ServiceExt;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::ControlMsg;
    use dormant_doctor::DoctorService;

    use crate::WebState;
    use crate::state::WebStateInner;

    /// Build a minimal [`WebState`] suitable for testing `build_router`.
    /// The API routes will fail if called (no real engine behind the
    /// channels), but the security guard and SPA fallback work
    /// independently of the engine.
    fn test_web_state_with_bind(bind: SocketAddr) -> (WebState, CancellationToken) {
        let cancel = CancellationToken::new();

        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);

        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let creds = Arc::new(Credentials::default());

        let (config_tx, config_rx) = tokio::sync::watch::channel(config);
        let (creds_tx, creds_rx) = tokio::sync::watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor = DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let state = WebState::new(WebStateInner {
            ctl_tx: ctl_tx.clone(),
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
        });

        (state, cancel)
    }

    // ── Security guard covers static/SPA paths ────────────────────────────

    #[tokio::test]
    async fn security_guard_rejects_foreign_host_on_root() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "GET / with foreign Host must be rejected (403)"
        );
    }

    #[tokio::test]
    async fn security_guard_rejects_foreign_host_on_spa_route() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/some/spa/route")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "SPA route with foreign Host must be rejected (403)"
        );
    }

    #[tokio::test]
    async fn security_guard_allows_loopback_host_on_root() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("Host", "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "GET / with loopback Host must succeed"
        );

        // Sanity: the response should be the SPA index (text/html).
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        assert_eq!(
            ct,
            Some("text/html"),
            "root with legit Host should serve text/html"
        );
    }
}
