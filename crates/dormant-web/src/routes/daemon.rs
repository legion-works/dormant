//! `GET /api/daemon` — daemon process identity for the sidebar footer.
//!
//! Additive, read-only endpoint: pid, process start time, build version,
//! and the resolved IPC socket path. None of these live on
//! [`dormant_core::rules::StateSnapshot`] — they are process/config facts,
//! not engine state, so this route reads [`WebState`] directly rather than
//! round-tripping the `ControlMsg` channel (mirrors the `/api/wear` and
//! `/api/operations` direct-read pattern).

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use serde::Serialize;

use crate::WebState;

/// Browser-visible daemon process identity.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct DaemonIdentity {
    /// The daemon process's OS pid.
    pub pid: u32,
    /// Epoch seconds when this `WebState` was constructed — a proxy for
    /// daemon start time (the web server starts during daemon `start()`,
    /// see `dormantd::app::App::start`).
    pub started_epoch_s: u64,
    /// Workspace crate version (`CARGO_PKG_VERSION` of this crate, which
    /// shares the workspace-unified version with `dormantd`).
    pub version: &'static str,
    /// Resolved IPC socket path — same resolution `dormantd` uses to spawn
    /// its own IPC listener (`dormant_core::paths::resolve_socket_path`).
    pub socket: String,
}

/// `GET /api/daemon` — report the daemon's process identity.
pub(crate) async fn get_daemon(State(state): State<WebState>) -> impl IntoResponse {
    let socket_config = state.inner.config_rx.borrow().daemon.socket_path.clone();
    let socket = dormant_core::paths::resolve_socket_path(socket_config.as_deref());

    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(DaemonIdentity {
            pid: std::process::id(),
            started_epoch_s: state.inner.started_epoch_s,
            version: env!("CARGO_PKG_VERSION"),
            socket: socket.display().to_string(),
        }),
    )
}
