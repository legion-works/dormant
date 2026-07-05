//! `WS /api/events` — WebSocket upgrade stub (full stream in Task 14).
//!
//! Task 14 builds the full event stream with re-subscription-on-reload,
//! streaming `DaemonEvent`, and `stream_lagged` handling.  This stub
//! accepts the WebSocket upgrade and immediately closes the connection
//! with a status message so the route exists and tests can mount it.

use axum::extract::ws::Message;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;

use crate::WebState;

/// Stub handler — accepts the upgrade, sends a "not yet implemented" close
/// frame, and returns.
pub(crate) async fn ws_events(
    State(_state): State<WebState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        let _ = socket
            .send(Message::Text(
                "WebSocket events stream — full implementation in Task 14".into(),
            ))
            .await;
        let _ = socket.close().await;
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    fn test_web_state_with_bind(bind: SocketAddr) -> WebState {
        let cancel = CancellationToken::new();
        let (ctl_tx, _ctl_rx) = mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) = mpsc::channel::<()>(8);
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
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        WebState::new(crate::state::WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            doctor,
            web_bind: bind,
            cancel,
        })
    }

    #[tokio::test]
    async fn ws_events_route_is_mountable() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let state = test_web_state_with_bind(bind);
        // The route compiles and mounts — full WS integration is tested in
        // Task 14 (real TCP upgrade + stream).
        let _app: Router = Router::new()
            .route("/api/events", get(ws_events))
            .with_state(state);
    }
}
