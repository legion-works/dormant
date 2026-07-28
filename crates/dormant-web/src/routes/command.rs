//! Command routes — `POST /api/blank|wake|pause|resume|reload`.
//!
//! All command routes validate display existence against a fresh engine
//! snapshot before dispatching, mirroring the IPC server's
//! [`validate_display_name`] pattern.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use dormant_core::rules::{ControlMsg, EmergencyWakeReport};
use dormant_core::types::{DisplayId, RuleId, Timestamp};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use crate::WebState;
use crate::error::WebError;

/// Maximum time to wait for a snapshot before rejecting a command.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time the web layer waits for a global emergency-wake report
/// before returning 504. The engine-side IPC bound
/// (`dormantd/src/ipc.rs::EMERGENCY_WAKE_IPC_TIMEOUT`) is also 2 s; kept in
/// sync deliberately, not derived, since the two crates do not share a
/// dependency edge for this constant.
const EMERGENCY_WAKE_WEB_TIMEOUT: Duration = Duration::from_secs(2);

// ── Request bodies ──────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub(crate) struct BlankBody {
    pub(crate) display: String,
    /// Blank mode — see [`dormant_core::ipc_proto::BlankRequestMode`].
    /// Defaults to `Soft`, matching the safety-first CLI and wire protocol.
    #[serde(default = "blank_body_default_mode_soft")]
    pub(crate) mode: dormant_core::ipc_proto::BlankRequestMode,
}

/// HTTP-body default for [`BlankBody::mode`].
fn blank_body_default_mode_soft() -> dormant_core::ipc_proto::BlankRequestMode {
    dormant_core::ipc_proto::BlankRequestMode::Soft
}

#[derive(Deserialize, Debug)]
pub(crate) struct WakeBody {
    pub(crate) display: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct SwitchBody {
    pub(crate) display: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct PauseBody {
    pub(crate) rule: Option<String>,
    /// Duration in seconds; `None` = indefinite.
    pub(crate) duration_s: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct ResumeBody {
    pub(crate) rule: Option<String>,
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// `POST /api/switch` — write the local input code to pull the display.
pub(crate) async fn post_switch(
    State(state): State<WebState>,
    Json(body): Json<SwitchBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    validate_display_exists(&state.inner.ctl_tx, &body.display).await?;
    let request = dormant_core::ipc_proto::IpcRequest::SwitchToLocal {
        display: body.display,
    };
    let response = crate::request_daemon_ipc(&state, request).await?;
    if !response.ok {
        return Err(WebError::BadRequest(
            response
                .error
                .unwrap_or_else(|| "switch failed".to_string()),
        ));
    }
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/push` — write the peer input code to push the display away.
pub(crate) async fn post_push(
    State(state): State<WebState>,
    Json(body): Json<SwitchBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    validate_display_exists(&state.inner.ctl_tx, &body.display).await?;
    let request = dormant_core::ipc_proto::IpcRequest::SwitchToPeer {
        display: body.display,
    };
    let response = crate::request_daemon_ipc(&state, request).await?;
    if !response.ok {
        return Err(WebError::BadRequest(
            response.error.unwrap_or_else(|| "push failed".to_string()),
        ));
    }
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/blank` — validate display exists, then dispatch soft or hard
/// per the request body's `mode` field.  Issue #124 split the policy: this
/// web route is the entry point for the webui's existing "Force blank"
/// button, so it forwards Hard by default.  Soft is reachable explicitly via
/// `{"display": "x", "mode": "soft"}` for any future soft-blank UI surface.
pub(crate) async fn post_blank(
    State(state): State<WebState>,
    Json(body): Json<BlankBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    validate_display_exists(&state.inner.ctl_tx, &body.display).await?;

    let msg = match body.mode {
        dormant_core::ipc_proto::BlankRequestMode::Soft => {
            ControlMsg::SoftBlank(DisplayId(body.display))
        }
        dormant_core::ipc_proto::BlankRequestMode::Hard => {
            ControlMsg::ForceBlank(DisplayId(body.display))
        }
    };
    state
        .inner
        .ctl_tx
        .send(msg)
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/wake` — validate display exists, then force-wake.
pub(crate) async fn post_wake(
    State(state): State<WebState>,
    Json(body): Json<WakeBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    validate_display_exists(&state.inner.ctl_tx, &body.display).await?;

    let msg = ControlMsg::ForceWake(DisplayId(body.display));
    state
        .inner
        .ctl_tx
        .send(msg)
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/pause` — pause blanking for one or all rules.
/// Duration overflow is rejected with 400 (mirroring `ipc.rs:270-275`).
pub(crate) async fn post_pause(
    State(state): State<WebState>,
    Json(body): Json<PauseBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    let until = match body.duration_s {
        Some(secs) => {
            match std::time::SystemTime::now().checked_add(std::time::Duration::from_secs(secs)) {
                Some(t) => Some(Timestamp(t)),
                None => {
                    return Err(WebError::BadRequest("duration overflow".into()));
                }
            }
        }
        None => None,
    };

    let msg = ControlMsg::Pause {
        rule: body.rule.map(RuleId),
        until,
    };
    state
        .inner
        .ctl_tx
        .send(msg)
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/resume` — resume blanking for one or all rules.
pub(crate) async fn post_resume(
    State(state): State<WebState>,
    Json(body): Json<ResumeBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    let msg = ControlMsg::Resume {
        rule: body.rule.map(RuleId),
    };
    state
        .inner
        .ctl_tx
        .send(msg)
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/reload` — trigger a config reload.
pub(crate) async fn post_reload(
    State(state): State<WebState>,
) -> Result<Json<serde_json::Value>, WebError> {
    state
        .inner
        .reload_requester
        .notify(dormant_core::observation::ReloadSource::Control)
        .await
        .then_some(())
        .ok_or(WebError::ReloadUnavailable)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/emergency-wake` — pause every rule and wake every display.
///
/// The route intentionally has no secondary fallback. It already runs inside
/// `dormantd` and shares the engine channel; a second wake path would race the
/// engine-owned all-rule pause and produce a less trustworthy report.
pub(crate) async fn post_emergency_wake(
    State(state): State<WebState>,
) -> Result<Json<EmergencyWakeReport>, WebError> {
    let guard = state
        .inner
        .emergency_wake_lock
        .clone()
        .try_lock_owned()
        .map_err(|_| WebError::EmergencyWakeInProgress)?;
    let (reply_tx, reply_rx) = oneshot::channel();

    // Spawn before the channel send: if the HTTP task is cancelled while send
    // is pending, this detached monitor still owns the web single-flight guard.
    let mut completion = tokio::spawn(async move {
        let result = reply_rx.await;
        drop(guard);
        result
    });
    state
        .inner
        .ctl_tx
        .send(ControlMsg::EmergencyWake { reply: reply_tx })
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    match tokio::time::timeout(EMERGENCY_WAKE_WEB_TIMEOUT, &mut completion).await {
        Ok(Ok(Ok(report))) => Ok(Json(report)),
        Ok(Ok(Err(_)) | Err(_)) => Err(WebError::EmergencyWakeCancelled),
        Err(_) => Err(WebError::EmergencyWakeReportTimeout),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Fetch a snapshot from the engine (bounded), mirroring `ipc.rs`'s
/// `request_snapshot`.
async fn request_snapshot(
    ctl_tx: &mpsc::Sender<ControlMsg>,
) -> Result<dormant_core::rules::StateSnapshot, WebError> {
    let (tx, rx) = oneshot::channel();
    ctl_tx
        .send(ControlMsg::Snapshot(tx))
        .await
        .map_err(|_| WebError::EngineUnavailable)?;
    tokio::time::timeout(SNAPSHOT_TIMEOUT, rx)
        .await
        .map_err(|_| WebError::SnapshotTimeout)?
        .map_err(|_| WebError::SnapshotCancelled)
}

/// Validate that a display name exists in the current engine snapshot.
/// Returns `WebError::UnknownDisplay` if the display is unknown, mirroring
/// the IPC server's `validate_display_name`.
pub(super) async fn validate_display_exists(
    ctl_tx: &mpsc::Sender<ControlMsg>,
    display: &str,
) -> Result<(), WebError> {
    let snap = request_snapshot(ctl_tx).await?;
    let known: std::collections::HashSet<&str> =
        snap.displays.iter().map(|(id, _)| id.as_str()).collect();
    if known.contains(display) {
        Ok(())
    } else {
        Err(WebError::UnknownDisplay(display.to_string()))
    }
}

// ── Module-private helpers exposed for router-level tests ────────────────────

/// Build a minimal axum [`Router`] with all command routes mounted, suitable
/// for HTTP-level tests via `oneshot`.
#[cfg(test)]
fn command_test_router(ctl_tx: mpsc::Sender<ControlMsg>) -> axum::Router {
    command_test_router_at(ctl_tx, None)
}

#[cfg(test)]
fn command_test_router_at(
    ctl_tx: mpsc::Sender<ControlMsg>,
    socket: Option<std::path::PathBuf>,
) -> axum::Router {
    use crate::state::{WebStateInner, WebStateInnerParams};
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::watch;

    let cancel = tokio_util::sync::CancellationToken::new();
    let (reload_trigger_tx, mut reload_trigger_rx) =
        mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
    let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
    let config = Arc::new(Config {
        coordination: dormant_core::config::CoordinationConfig::default(),
        config_version: 1,
        daemon: DaemonConfig {
            socket_path: socket,
            ..DaemonConfig::default()
        },
        wear: dormant_core::config::schema::WearConfig::default(),
        notifications: dormant_core::config::schema::NotificationsConfig::default(),
        watchdog: dormant_core::config::schema::WatchdogConfig::default(),
        audio: dormant_core::config::schema::AudioConfig::default(),
        sensors: IndexMap::default(),
        zones: IndexMap::default(),
        displays: IndexMap::default(),
        rules: IndexMap::default(),
        keymap: dormant_core::config::KeymapConfig::default(),
        input_filter: dormant_core::config::InputFilterConfig::default(),
        publish: dormant_core::config::PublishConfig::default(),
    });
    let creds = Arc::new(Credentials::default());
    let (config_tx, config_rx) = watch::channel(config);
    let (creds_tx, creds_rx) = watch::channel(creds);

    std::mem::forget(reload_tx);
    std::mem::forget(config_tx);
    std::mem::forget(creds_tx);

    let doctor =
        dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

    let state = WebState::new(WebStateInner::new_for_test(WebStateInnerParams {
        ctl_tx,
        reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
        reload_rx,
        config_rx,
        creds_rx,
        config_path: std::path::PathBuf::from("/dev/null"),
        creds_path: std::path::PathBuf::from("/dev/null"),
        doctor,
        wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
        cancel,
        reload_timeout: Duration::from_secs(10),
    }));

    // Keep the reload trigger receiver alive in a spawned task so the
    // sender never errors.
    tokio::spawn(async move {
        let _ = reload_trigger_rx.recv().await;
    });

    axum::Router::new()
        .route("/api/blank", axum::routing::post(post_blank))
        .route("/api/wake", axum::routing::post(post_wake))
        .route("/api/switch", axum::routing::post(post_switch))
        .route("/api/push", axum::routing::post(post_push))
        .route("/api/pause", axum::routing::post(post_pause))
        .route("/api/resume", axum::routing::post(post_resume))
        .route("/api/reload", axum::routing::post(post_reload))
        .route(
            "/api/emergency-wake",
            axum::routing::post(post_emergency_wake),
        )
        .with_state(state)
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::response::IntoResponse;
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::watch;
    use tower::util::ServiceExt;

    /// Build a fake engine that responds to `ControlMsg::Snapshot` and records
    /// the LAST non-snapshot `ControlMsg` it received.
    fn spawn_fake_engine(
        snapshot: StateSnapshot,
    ) -> (
        mpsc::Sender<ControlMsg>,
        Arc<std::sync::Mutex<Option<ControlMsg>>>,
    ) {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
        let last_msg = Arc::new(std::sync::Mutex::new(None));
        let record = last_msg.clone();
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::Snapshot(tx) => {
                        let _ = tx.send(snapshot.clone());
                    }
                    other => {
                        *record.lock().unwrap() = Some(other);
                    }
                }
            }
        });
        (ctl_tx, last_msg)
    }

    fn test_web_state(ctl_tx: mpsc::Sender<ControlMsg>) -> WebState {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (reload_trigger_tx, _reload_trigger_rx) =
            mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx,
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path: std::path::PathBuf::from("/dev/null"),
                creds_path: std::path::PathBuf::from("/dev/null"),
                doctor,
                wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
                cancel,
                reload_timeout: Duration::from_secs(10),
            },
        ))
    }

    fn snapshot_with_displays(ids: &[&str]) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: ids
                .iter()
                .map(|id| {
                    (
                        id.to_string(),
                        DisplaySnapshot {
                            phase: "active".into(),
                            inhibited: false,
                            paused: false,
                            cmd_gen: 1,
                            scope: dormant_core::config::DisplayScope::Private,
                            owned: true,
                            observed_input_code: None,
                            panel_state: None,
                            controllers: vec![],
                            wake_attempts: 0,
                            last_blank_failed: false,
                            stage: None,
                        },
                    )
                })
                .collect(),
            pending_reload: None,
            rollback: None,
            kvm: None,
        }
    }

    // ── Blank / Wake tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn blank_known_display_sends_force_blank() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_blank(
            State(state),
            Json(BlankBody {
                display: "main".into(),
                mode: dormant_core::ipc_proto::BlankRequestMode::Hard,
            }),
        )
        .await;
        assert!(result.is_ok(), "blank should succeed: {:?}", result.err());

        tokio::task::yield_now().await;

        let msg = last_msg.lock().unwrap().take();
        assert!(
            matches!(msg, Some(ControlMsg::ForceBlank(ref id)) if id.0 == "main"),
            "expected ForceBlank(main), got {msg:?}"
        );
    }

    /// `POST /api/blank` with `mode: "soft"` must dispatch `ControlMsg::SoftBlank`
    /// — the safe ladder path.  The webui does not yet ship a "soft blank"
    /// surface, but the route contract pins the behaviour so a future
    /// soft-blank UI cannot regress into `ForceBlank`.
    #[tokio::test]
    async fn blank_soft_mode_sends_soft_blank() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_blank(
            State(state),
            Json(BlankBody {
                display: "main".into(),
                mode: dormant_core::ipc_proto::BlankRequestMode::Soft,
            }),
        )
        .await;
        assert!(
            result.is_ok(),
            "soft blank should succeed: {:?}",
            result.err()
        );

        tokio::task::yield_now().await;

        let msg = last_msg.lock().unwrap().take();
        assert!(
            matches!(msg, Some(ControlMsg::SoftBlank(ref id)) if id.0 == "main"),
            "expected SoftBlank(main), got {msg:?}"
        );
    }

    /// `POST /api/blank` with no `mode` field defaults to the safe Soft path.
    #[tokio::test]
    async fn blank_no_mode_defaults_to_soft() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let body: BlankBody = serde_json::from_str(r#"{"display":"main"}"#).unwrap();
        assert_eq!(
            body.mode,
            dormant_core::ipc_proto::BlankRequestMode::Soft,
            "missing mode field must default to Soft"
        );

        let result = post_blank(State(state), Json(body)).await;
        assert!(result.is_ok(), "blank should succeed: {:?}", result.err());

        tokio::task::yield_now().await;

        let msg = last_msg.lock().unwrap().take();
        assert!(
            matches!(msg, Some(ControlMsg::SoftBlank(ref id)) if id.0 == "main"),
            "expected SoftBlank(main) for the no-mode frame, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn blank_unknown_display_returns_404() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, _last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_blank(
            State(state),
            Json(BlankBody {
                display: "bogus".into(),
                mode: dormant_core::ipc_proto::BlankRequestMode::Hard,
            }),
        )
        .await;

        match result {
            Err(WebError::UnknownDisplay(name)) => assert_eq!(name, "bogus"),
            other => panic!("expected UnknownDisplay, got {other:?}"),
        }
    }

    /// Router-level test: unknown display → HTTP 404 status.
    #[tokio::test]
    async fn unknown_display_returns_http_404() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let router = command_test_router(ctl_tx);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/blank")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"display":"bogus"}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wake_known_display_sends_force_wake() {
        let snap = snapshot_with_displays(&["tv"]);
        let (ctl_tx, last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_wake(
            State(state),
            Json(WakeBody {
                display: "tv".into(),
            }),
        )
        .await;
        assert!(result.is_ok());

        tokio::task::yield_now().await;

        let msg = last_msg.lock().unwrap().take();
        assert!(
            matches!(msg, Some(ControlMsg::ForceWake(ref id)) if id.0 == "tv"),
            "expected ForceWake(tv), got {msg:?}"
        );
    }

    #[tokio::test]
    async fn wake_unknown_display_returns_404() {
        let snap = snapshot_with_displays(&["tv"]);
        let (ctl_tx, _last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_wake(
            State(state),
            Json(WakeBody {
                display: "bogus".into(),
            }),
        )
        .await;

        match result {
            Err(WebError::UnknownDisplay(name)) => assert_eq!(name, "bogus"),
            other => panic!("expected UnknownDisplay, got {other:?}"),
        }
    }

    // ── Switch-to-local tests ──────────────────────────────────────────

    /// Router-level: `POST /api/switch` with `{"display":"shared"}`
    /// sends `SwitchToLocal` and returns `{"status":"ok"}` on success.
    #[tokio::test]
    async fn switch_sends_switch_to_local_and_returns_ok() {
        let snap = snapshot_with_displays(&["shared"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut request)
                .unwrap();
            assert!(
                request.contains("switch_to_local"),
                "expected switch_to_local, got: {request}"
            );
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        });
        let router = command_test_router_at(ctl_tx, Some(socket));
        tokio::task::yield_now().await;
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/switch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"display":"shared"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        server.await.unwrap();
    }

    /// Router-level: `POST /api/switch` surfaces a daemon error as HTTP 400.
    #[tokio::test]
    async fn switch_surfaces_failed_write() {
        let snap = snapshot_with_displays(&["shared"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut request)
                .unwrap();
            stream
                .write_all(b"{\"ok\":false,\"error\":\"write failed: DDC bus unreachable\"}\n")
                .unwrap();
        });
        let router = command_test_router_at(ctl_tx, Some(socket));
        tokio::task::yield_now().await;
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/switch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"display":"shared"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["detail"].as_str().unwrap().contains("DDC bus"));
        server.await.unwrap();
    }

    /// Router-level: `POST /api/switch` with an unknown display returns 404.
    #[tokio::test]
    async fn switch_unknown_display_returns_404() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let router = command_test_router(ctl_tx);
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/switch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"display":"bogus"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Push-to-peer tests ──────────────────────────────────────────

    /// Router-level: `POST /api/push` with `{"display":"shared"}`
    /// sends `SwitchToPeer` (NOT `SwitchToLocal`) and returns `{"status":"ok"}` on success.
    #[tokio::test]
    async fn push_sends_switch_to_peer_and_returns_ok() {
        let snap = snapshot_with_displays(&["shared"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut request)
                .unwrap();
            assert!(
                request.contains("switch_to_peer"),
                "expected switch_to_peer, got: {request}"
            );
            assert!(
                !request.contains("switch_to_local"),
                "unexpected switch_to_local in push request: {request}"
            );
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        });
        let router = command_test_router_at(ctl_tx, Some(socket));
        tokio::task::yield_now().await;
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/push")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"display":"shared"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        server.await.unwrap();
    }

    /// Router-level: `POST /api/push` surfaces a daemon error as HTTP 400.
    #[tokio::test]
    async fn push_surfaces_failed_write() {
        let snap = snapshot_with_displays(&["shared"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dormant.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut request)
                .unwrap();
            stream
                .write_all(b"{\"ok\":false,\"error\":\"write failed: DDC bus unreachable\"}\n")
                .unwrap();
        });
        let router = command_test_router_at(ctl_tx, Some(socket));
        tokio::task::yield_now().await;
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/push")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"display":"shared"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["detail"].as_str().unwrap().contains("DDC bus"));
        server.await.unwrap();
    }

    /// Router-level: `POST /api/push` with an unknown display returns 404.
    #[tokio::test]
    async fn push_unknown_display_returns_404() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let router = command_test_router(ctl_tx);
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/push")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"display":"bogus"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Pause / Resume tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn pause_sends_control_msg() {
        let snap = snapshot_with_displays(&[]);
        let (ctl_tx, last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_pause(
            State(state),
            Json(PauseBody {
                rule: Some("living_room".into()),
                duration_s: Some(60),
            }),
        )
        .await;
        assert!(result.is_ok(), "pause should succeed: {:?}", result.err());

        tokio::task::yield_now().await;

        let msg = last_msg.lock().unwrap().take();
        assert!(
            matches!(&msg, Some(ControlMsg::Pause { rule, until: Some(_) })
                if rule.as_ref().map(|r| r.0.as_str()) == Some("living_room")),
            "expected Pause with rule=living_room + until, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn pause_duration_overflow_returns_400() {
        let snap = snapshot_with_displays(&[]);
        let (ctl_tx, _last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        // u64::MAX seconds will overflow SystemTime.
        let result = post_pause(
            State(state),
            Json(PauseBody {
                rule: Some("test".into()),
                duration_s: Some(u64::MAX),
            }),
        )
        .await;

        match result {
            Err(WebError::BadRequest(msg)) => assert!(
                msg.contains("overflow"),
                "expected overflow message, got: {msg}"
            ),
            other => panic!("expected BadRequest(overflow), got {other:?}"),
        }
    }

    /// Router-level test: overflow → HTTP 400 status.
    #[tokio::test]
    async fn pause_overflow_returns_http_400() {
        let snap = snapshot_with_displays(&[]);
        let (ctl_tx, _) = spawn_fake_engine(snap);
        let router = command_test_router(ctl_tx);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/pause")
            .header("Content-Type", "application/json")
            .body(Body::from(
                r#"{"rule":"test","duration_s":18446744073709551615}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn resume_sends_control_msg() {
        let snap = snapshot_with_displays(&[]);
        let (ctl_tx, last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_resume(
            State(state),
            Json(ResumeBody {
                rule: Some("living_room".into()),
            }),
        )
        .await;
        assert!(result.is_ok());

        tokio::task::yield_now().await;

        let msg = last_msg.lock().unwrap().take();
        assert!(
            matches!(&msg, Some(ControlMsg::Resume { rule })
                if rule.as_ref().map(|r| r.0.as_str()) == Some("living_room")),
            "expected Resume with rule=living_room, got {msg:?}"
        );
    }

    // ── Reload tests ──────────────────────────────────────────────────────

    /// Router-level test: `POST /api/reload` triggers the `reload_trigger`.
    #[tokio::test]
    async fn reload_triggers_reload_sender() {
        let snap = snapshot_with_displays(&[]);
        let (ctl_tx, _) = spawn_fake_engine(snap);

        // Build a custom state with a reload_trigger that can be observed.
        let cancel = tokio_util::sync::CancellationToken::new();
        let (reload_trigger_tx, mut reload_trigger_rx) =
            mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let state = WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx: ctl_tx.clone(),
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path: std::path::PathBuf::from("/dev/null"),
                creds_path: std::path::PathBuf::from("/dev/null"),
                doctor,
                wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
                cancel,
                reload_timeout: Duration::from_secs(10),
            },
        ));

        let router = axum::Router::new()
            .route("/api/reload", axum::routing::post(post_reload))
            .with_state(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/reload")
            .header("Content-Type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The reload trigger should have received the signal.
        let triggered = tokio::time::timeout(Duration::from_millis(100), reload_trigger_rx.recv())
            .await
            .is_ok();
        assert!(triggered, "reload trigger should have been signalled");
    }

    // ── Emergency-wake tests ────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn emergency_wake_times_out_after_two_seconds() {
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let state = test_web_state(ctl_tx);
        let err = post_emergency_wake(State(state)).await.unwrap_err();
        assert!(matches!(err, WebError::EmergencyWakeReportTimeout));
    }

    #[tokio::test]
    async fn emergency_wake_dropped_reply_maps_to_500_json() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let Some(ControlMsg::EmergencyWake { reply }) = ctl_rx.recv().await else {
                panic!("expected EmergencyWake");
            };
            drop(reply);
        });
        let response = command_test_router(ctl_tx)
            .oneshot(
                Request::post("/api/emergency-wake")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "emergency_wake_cancelled"})
        );
    }

    #[tokio::test]
    async fn concurrent_emergency_wake_is_rejected_while_first_reply_is_pending() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let state = test_web_state(ctl_tx);
        let first_state = state.clone();
        let first = tokio::spawn(async move { post_emergency_wake(State(first_state)).await });
        let Some(ControlMsg::EmergencyWake { reply }) = ctl_rx.recv().await else {
            panic!("expected first EmergencyWake");
        };

        let second = post_emergency_wake(State(state)).await.unwrap_err();
        assert!(matches!(second, WebError::EmergencyWakeInProgress));
        let _ = reply.send(EmergencyWakeReport {
            operation_id: None,
            generation: None,
            paused: true,
            displays: Vec::new(),
        });
        let _ = first.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn emergency_wake_guard_survives_http_report_timeout() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let state = test_web_state(ctl_tx);
        let first_state = state.clone();
        let first = tokio::spawn(async move { post_emergency_wake(State(first_state)).await });
        let reply = match ctl_rx.recv().await.unwrap() {
            ControlMsg::EmergencyWake { reply } => reply,
            other => panic!("expected EmergencyWake, got {other:?}"),
        };
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(
            first.await.unwrap().unwrap_err(),
            WebError::EmergencyWakeReportTimeout
        ));
        assert!(matches!(
            post_emergency_wake(State(state)).await.unwrap_err(),
            WebError::EmergencyWakeInProgress
        ));
        drop(reply);
    }

    #[tokio::test]
    async fn emergency_wake_cancelled_maps_to_500_json_exactly() {
        let response = WebError::EmergencyWakeCancelled.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "emergency_wake_cancelled"})
        );
    }

    #[tokio::test]
    async fn emergency_wake_report_timeout_maps_to_504_json_exactly() {
        let response = WebError::EmergencyWakeReportTimeout.into_response();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "emergency_wake_report_timeout"})
        );
    }

    #[tokio::test]
    async fn emergency_wake_in_progress_maps_to_409_json_exactly() {
        let response = WebError::EmergencyWakeInProgress.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "emergency_wake_in_progress"})
        );
    }
}
