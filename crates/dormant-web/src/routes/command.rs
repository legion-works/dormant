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
pub(crate) async fn post_pause(
    State(state): State<WebState>,
    Json(body): Json<PauseBody>,
) -> Result<Json<serde_json::Value>, WebError> {
    let until = body.duration_s.map(|secs| {
        let future = std::time::SystemTime::now()
            .checked_add(std::time::Duration::from_secs(secs))
            .unwrap_or(std::time::SystemTime::now());
        Timestamp(future)
    });

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

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::{DisplaySnapshot, StateSnapshot};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::watch;

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
            web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            cancel,
        })
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

        // Yield so the fake engine task processes queued messages.
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
}
