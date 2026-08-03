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
    /// Whether the "Star the repo" sidebar nudge has been dismissed.
    /// Persisted as a flag file (`star-nudge-dismissed`) in the config directory.
    /// Defaults to `false` when absent — older clients and first-load treat
    /// an unknown/missing key as not-yet-dismissed.
    #[serde(default)]
    pub star_nudge_dismissed: bool,
    /// Whether the daemon's active wear-sampling pipeline is **platform
    /// capable** — i.e. the daemon actually owns a sampler lifecycle on
    /// this host. Derived from `wear_sampling_rx.borrow().is_some()`:
    /// non-Linux builds never spawn the active sampler (the module is
    /// `#[cfg(target_os = "linux")]`), so the watch stays `None` and
    /// this is `false`. Critically, this is independent of the user's
    /// `wear.active_sampling.enabled` config flag — that is the user's
    /// *intent*, this is the system's *capability*. The wear-card
    /// onboarding nudge (#186) must read this field, not the config
    /// flag, to decide whether to show the portal action.
    #[serde(default)]
    pub wear_sampling_supported: bool,
    /// Whether the wear-card onboarding nudge has been dismissed.
    /// Persisted as a flag file (`wear-sampling-nudge-dismissed`) in the
    /// config directory. Defaults to `false` when absent — older clients
    /// and first-load treat an unknown/missing key as not-yet-dismissed.
    #[serde(default)]
    pub wear_sampling_nudge_dismissed: bool,
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
            star_nudge_dismissed: state.inner.star_nudge_path.exists(),
            // The daemon only writes to wear_sampling_rx on Linux (the
            // active_sampler module is `#[cfg(target_os = "linux")]`); on
            // any other host the watch stays `None` and the platform is
            // incapable of running active sampling. The web UI uses this
            // signal to gate the portal-consent affordance — the config
            // `enabled` flag is the user's *intent*, not the system's
            // *capability* (see #186).
            wear_sampling_supported: state.inner.wear_sampling_rx.borrow().is_some(),
            wear_sampling_nudge_dismissed: state.inner.wear_sampling_nudge_path.exists(),
        }),
    )
}
