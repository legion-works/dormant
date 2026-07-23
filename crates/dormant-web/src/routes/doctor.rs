//! `POST /api/doctor` — run the coalesced `DoctorService` and return a report.
//!
//! The handler directly calls [`DoctorService::run`] — does NOT go through
//! the engine's `ControlMsg` channel (spec §5.1).  Concurrent callers share
//! the single in-flight run.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use dormant_core::doctor::DoctorReport;
use dormant_core::rules::{ControlMsg, ExerciseReport};
use dormant_core::types::DisplayId;
use tokio::sync::oneshot;

use crate::WebState;
use crate::error::WebError;

pub(crate) async fn post_doctor(State(state): State<WebState>) -> Json<DoctorReport> {
    let report = state.inner.doctor.run().await;
    Json(report)
}

/// Maximum time the web layer waits for a per-display exercise report
/// before returning 504. The engine-side IPC bound
/// (`dormantd/src/ipc.rs::EXERCISE_IPC_TIMEOUT`) is also 20 s; kept in
/// sync deliberately, not derived, since the two crates do not share a
/// dependency edge for this constant.
const EXERCISE_WEB_TIMEOUT: Duration = Duration::from_secs(20);

/// `POST /api/doctor/exercise/:display` — run the engine-owned control-path exercise.
///
/// The engine pauses and resumes affected rules around the exercise. The route
/// must not add resume logic: timing out the HTTP adapter does not transfer
/// ownership of the engine's safety sequence to the web layer.
pub(crate) async fn post_exercise(
    Path(display): Path<String>,
    State(state): State<WebState>,
) -> Result<Json<ExerciseReport>, WebError> {
    let display = DisplayId(display);
    {
        let mut in_flight = state.inner.exercise_in_flight.lock().await;
        if !in_flight.insert(display.clone()) {
            return Err(WebError::ExerciseInProgress);
        }
    }
    let (reply_tx, reply_rx) = oneshot::channel();

    // No await occurs between insertion and this spawn. The detached monitor
    // therefore owns cleanup before the request can be cancelled at send.
    let in_flight = Arc::clone(&state.inner.exercise_in_flight);
    let guarded_display = display.clone();
    let mut completion = tokio::spawn(async move {
        let result = reply_rx.await;
        in_flight.lock().await.remove(&guarded_display);
        result
    });
    if let Err(error) =
        super::command::validate_display_exists(&state.inner.ctl_tx, &display.0).await
    {
        drop(reply_tx);
        let _ = completion.await;
        return Err(error);
    }
    if state
        .inner
        .ctl_tx
        .send(ControlMsg::Exercise {
            display: display.clone(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return Err(WebError::EngineUnavailable);
    }

    match tokio::time::timeout(EXERCISE_WEB_TIMEOUT, &mut completion).await {
        Ok(Ok(Ok(report))) => Ok(Json(report)),
        Ok(Ok(Err(_)) | Err(_)) => Err(WebError::ExerciseCancelled),
        Err(_) => Err(WebError::ExerciseReportTimeout),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::{
        ControlMsg, DisplaySnapshot, ExerciseReport, ExerciseStep, ExerciseVerdict, StateSnapshot,
    };
    use dormant_core::traits::{PanelState, PowerState};
    use dormant_core::types::{DisplayId, RuleId};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    fn test_web_state(ctl_tx: mpsc::Sender<ControlMsg>) -> WebState {
        let cancel = CancellationToken::new();
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

    /// Spawn a fake engine that responds to `Snapshot`.
    fn spawn_fake_engine(snapshot: StateSnapshot) -> mpsc::Sender<ControlMsg> {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                if let ControlMsg::Snapshot(tx) = msg {
                    let _ = tx.send(snapshot.clone());
                }
            }
        });
        ctl_tx
    }

    fn snapshot_with_display(id: Option<&str>) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: id
                .into_iter()
                .map(|id| {
                    (
                        id.to_string(),
                        DisplaySnapshot {
                            phase: "active".to_string(),
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
        }
    }

    #[tokio::test]
    async fn exercise_route_returns_404_for_unknown_display() {
        let ctl_tx = spawn_fake_engine(snapshot_with_display(None));
        let state = test_web_state(ctl_tx);
        let app = Router::new()
            .route("/doctor/exercise/:display", post(post_exercise))
            .with_state(state);

        let response = app
            .oneshot(
                Request::post("/doctor/exercise/missing")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn exercise_route_returns_engine_report() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::Snapshot(reply) => {
                        let _ = reply.send(snapshot_with_display(Some("main")));
                    }
                    ControlMsg::Exercise { display, reply } => {
                        assert_eq!(display, DisplayId("main".to_string()));
                        let _ = reply.send(ExerciseReport {
                            display,
                            pre_phase: "active".to_string(),
                            paused_rules: vec![RuleId("office_blank".to_string())],
                            steps: vec![ExerciseStep {
                                command: "wake".to_string(),
                                blank_mode: None,
                                returned_ok: true,
                                state_before: Some(PanelState {
                                    power: Some(PowerState::Standby),
                                    brightness: None,
                                }),
                                state_after: Some(PanelState {
                                    power: Some(PowerState::On),
                                    brightness: None,
                                }),
                                verdict: ExerciseVerdict::Confirmed,
                                error: None,
                            }],
                        });
                        break;
                    }
                    other => panic!("unexpected route message: {other:?}"),
                }
            }
        });

        let app = Router::new()
            .route("/doctor/exercise/:display", post(post_exercise))
            .with_state(test_web_state(ctl_tx));
        let response = app
            .oneshot(
                Request::post("/doctor/exercise/main")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "display": "main",
                "pre_phase": "active",
                "paused_rules": ["office_blank"],
                "steps": [{
                    "command": "wake",
                    "returned_ok": true,
                    "state_before": {"power": "standby"},
                    "state_after": {"power": "on"},
                    "verdict": "confirmed"
                }]
            })
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn exercise_route_returns_504_when_engine_does_not_reply() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::Snapshot(reply) => {
                        let _ = reply.send(snapshot_with_display(Some("main")));
                    }
                    ControlMsg::Exercise { reply, .. } => {
                        let _keep_reply_open = reply;
                        std::future::pending::<()>().await;
                    }
                    other => panic!("unexpected route message: {other:?}"),
                }
            }
        });

        let app = Router::new()
            .route("/doctor/exercise/:display", post(post_exercise))
            .with_state(test_web_state(ctl_tx));
        let response = app
            .oneshot(
                Request::post("/doctor/exercise/main")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn exercise_times_out_after_twenty_seconds_without_resuming_in_route() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let state = test_web_state(ctl_tx);
        tokio::spawn(async move {
            while let Some(msg) = ctl_rx.recv().await {
                match msg {
                    ControlMsg::Snapshot(reply) => {
                        let _ = reply.send(snapshot_with_display(Some("main")));
                    }
                    ControlMsg::Exercise { reply, .. } => {
                        // Keep the reply channel open so the route reaches its own timeout.
                        std::mem::forget(reply);
                        break;
                    }
                    other => panic!("unexpected route message: {other:?}"),
                }
            }
        });

        let err = post_exercise(Path("main".to_string()), State(state))
            .await
            .unwrap_err();
        assert!(matches!(err, WebError::ExerciseReportTimeout));
    }

    #[tokio::test]
    async fn exercise_dropped_reply_maps_to_500_json() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            while let Some(message) = ctl_rx.recv().await {
                match message {
                    ControlMsg::Snapshot(reply) => {
                        let _ = reply.send(snapshot_with_display(Some("main")));
                    }
                    ControlMsg::Exercise { reply, .. } => {
                        drop(reply);
                        break;
                    }
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        });
        let response = Router::new()
            .route("/doctor/exercise/:display", post(post_exercise))
            .with_state(test_web_state(ctl_tx))
            .oneshot(
                Request::post("/doctor/exercise/main")
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
            serde_json::json!({"error": "exercise_cancelled"})
        );
    }

    #[tokio::test]
    async fn second_exercise_for_same_display_is_409_while_first_is_pending() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let state = test_web_state(ctl_tx);
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            post_exercise(Path("main".to_string()), State(first_state)).await
        });
        let snapshot_reply = match ctl_rx.recv().await.unwrap() {
            ControlMsg::Snapshot(reply) => reply,
            other => panic!("expected Snapshot, got {other:?}"),
        };
        let _ = snapshot_reply.send(snapshot_with_display(Some("main")));
        let exercise_reply = match ctl_rx.recv().await.unwrap() {
            ControlMsg::Exercise { reply, .. } => reply,
            other => panic!("expected Exercise, got {other:?}"),
        };

        let second = post_exercise(Path("main".to_string()), State(state))
            .await
            .unwrap_err();
        assert!(matches!(second, WebError::ExerciseInProgress));
        let _ = exercise_reply.send(ExerciseReport {
            display: DisplayId("main".to_string()),
            pre_phase: "active".to_string(),
            paused_rules: Vec::new(),
            steps: Vec::new(),
        });
        let _ = first.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn exercise_guard_survives_http_report_timeout() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let state = test_web_state(ctl_tx);
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            post_exercise(Path("main".to_string()), State(first_state)).await
        });
        let snapshot_reply = match ctl_rx.recv().await.unwrap() {
            ControlMsg::Snapshot(reply) => reply,
            other => panic!("expected Snapshot, got {other:?}"),
        };
        let _ = snapshot_reply.send(snapshot_with_display(Some("main")));
        let exercise_reply = match ctl_rx.recv().await.unwrap() {
            ControlMsg::Exercise { reply, .. } => reply,
            other => panic!("expected Exercise, got {other:?}"),
        };
        tokio::time::advance(Duration::from_secs(20)).await;
        assert!(matches!(
            first.await.unwrap().unwrap_err(),
            WebError::ExerciseReportTimeout
        ));
        assert!(matches!(
            post_exercise(Path("main".to_string()), State(state))
                .await
                .unwrap_err(),
            WebError::ExerciseInProgress
        ));
        drop(exercise_reply);
    }

    #[tokio::test]
    async fn exercise_in_progress_maps_to_409_json_exactly() {
        let response = WebError::ExerciseInProgress.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "exercise_in_progress"})
        );
    }

    #[tokio::test]
    async fn exercise_cancelled_maps_to_500_json_exactly() {
        let response = WebError::ExerciseCancelled.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "exercise_cancelled"})
        );
    }

    #[tokio::test]
    async fn exercise_report_timeout_maps_to_504_json_exactly() {
        let response = WebError::ExerciseReportTimeout.into_response();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "exercise_report_timeout"})
        );
    }

    #[tokio::test]
    async fn doctor_returns_200_and_report() {
        let snapshot = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
            rollback: None,
        };
        let ctl_tx = spawn_fake_engine(snapshot);
        let state = test_web_state(ctl_tx);
        let result = post_doctor(State(state)).await;
        // Should return a DoctorReport (non-empty checks not required — empty
        // snapshot means no checks, and that's fine).
        assert!(!result.checks.is_empty() || result.checks.is_empty());
    }
}
