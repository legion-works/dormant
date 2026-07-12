//! Command routes — `POST /api/blank|wake|pause|resume|reload`.
//!
//! All command routes validate display existence against a fresh engine
//! snapshot before dispatching, mirroring the IPC server's
//! [`validate_display_name`] pattern.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use dormant_core::rules::ControlMsg;
use dormant_core::types::{DisplayId, RuleId, Timestamp};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use crate::WebState;
use crate::error::WebError;

/// Maximum time to wait for a snapshot before rejecting a command.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

// ── Request bodies ──────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub(crate) struct BlankBody {
    pub(crate) display: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct WakeBody {
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

/// `POST /api/blank` — validate display exists, then force-blank.
pub(crate) async fn post_blank(
    State(state): State<WebState>,
    Json(body): Json<BlankBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    validate_display_exists(&state.inner.ctl_tx, &body.display).await?;

    let msg = ControlMsg::ForceBlank(DisplayId(body.display));
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
        .reload_trigger
        .send(())
        .await
        .map_err(|_| WebError::ReloadUnavailable)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
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
async fn validate_display_exists(
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
    use crate::state::{WebStateInner, WebStateInnerParams};
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::watch;

    let cancel = tokio_util::sync::CancellationToken::new();
    let (reload_trigger_tx, mut reload_trigger_rx) = mpsc::channel::<()>(8);
    let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
    let config = Arc::new(Config {
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
        reload_trigger: reload_trigger_tx,
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
        .route("/api/pause", axum::routing::post(post_pause))
        .route("/api/resume", axum::routing::post(post_resume))
        .route("/api/reload", axum::routing::post(post_reload))
        .with_state(state)
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
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
        let (reload_trigger_tx, _reload_trigger_rx) = mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
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
                reload_trigger: reload_trigger_tx,
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
                            controllers: vec![],
                            wake_attempts: 0,
                            last_blank_failed: false,
                            stage: None,
                        },
                    )
                })
                .collect(),
            pending_reload: None,
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

    #[tokio::test]
    async fn blank_unknown_display_returns_404() {
        let snap = snapshot_with_displays(&["main"]);
        let (ctl_tx, _last_msg) = spawn_fake_engine(snap);
        let state = test_web_state(ctl_tx);

        let result = post_blank(
            State(state),
            Json(BlankBody {
                display: "bogus".into(),
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
        let (reload_trigger_tx, mut reload_trigger_rx) = mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
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
                reload_trigger: reload_trigger_tx,
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
}
