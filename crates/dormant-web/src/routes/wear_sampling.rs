//! Token-free HTTP forwarding for active wear-sampling consent flows.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dormant_core::ipc_proto::{IpcRequest, IpcResponse, WearSamplingStatus};

use crate::error::WebError;
use crate::{WebState, request_daemon_ipc};

fn status(response: IpcResponse) -> Result<WearSamplingStatus, WebError> {
    response
        .wear_sampling
        .ok_or(WebError::CoordinationUnavailable)
}

/// `POST /api/wear/sampling/enable` — start consent without waiting for its portal window.
pub(crate) async fn post_enable(
    State(state): State<WebState>,
) -> Result<(StatusCode, Json<WearSamplingStatus>), WebError> {
    let Ok(guard) = std::sync::Arc::clone(&state.inner.wear_sampling_lock).try_lock_owned() else {
        return Ok((
            StatusCode::CONFLICT,
            Json(WearSamplingStatus::Error(
                "wear_sampling_in_progress".to_owned(),
            )),
        ));
    };
    tokio::spawn(async move {
        let _guard = guard;
        let _ = request_daemon_ipc(&state, IpcRequest::WearSamplingEnable).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(WearSamplingStatus::AwaitingConsent),
    ))
}

/// `GET /api/wear/sampling` — poll the daemon-owned consent status.
pub(crate) async fn get_status(
    State(state): State<WebState>,
) -> Result<Json<WearSamplingStatus>, WebError> {
    let response = request_daemon_ipc(&state, IpcRequest::WearSamplingStatus).await?;
    Ok(Json(status(response)?))
}

/// `POST /api/wear/sampling/disable` — close sampling, optionally forgetting consent.
pub(crate) async fn post_disable(
    State(state): State<WebState>,
    Json(request): Json<DisableRequest>,
) -> Result<Json<WearSamplingStatus>, WebError> {
    let response = request_daemon_ipc(
        &state,
        IpcRequest::WearSamplingDisable {
            forget: request.forget,
        },
    )
    .await?;
    Ok(Json(status(response)?))
}

#[derive(serde::Deserialize)]
pub(crate) struct DisableRequest {
    pub(crate) forget: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::Arc;

    use dormant_core::config::schema::{
        AudioConfig, Config, Credentials, DaemonConfig, InputFilterConfig, KeymapConfig,
        NotificationsConfig, PublishConfig, WatchdogConfig, WearConfig,
    };
    use dormant_core::rules::ControlMsg;
    use dormant_doctor::DoctorService;
    use indexmap::IndexMap;
    use tokio::sync::{Mutex, Notify, broadcast, mpsc, watch};

    use crate::state::{DaemonIpc, WebStateInner, WebStateInnerParams};

    struct FakeIpc {
        requests: Mutex<Vec<IpcRequest>>,
        responses: Mutex<VecDeque<IpcResponse>>,
        enable_started: Notify,
        hold_enable: bool,
    }

    impl DaemonIpc for FakeIpc {
        fn request<'a>(
            &'a self,
            _socket: PathBuf,
            request: IpcRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<IpcResponse, WebError>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.requests.lock().await.push(request.clone());
                if self.hold_enable && request == IpcRequest::WearSamplingEnable {
                    self.enable_started.notify_waiters();
                    std::future::pending::<()>().await;
                }
                if request == IpcRequest::WearSamplingEnable {
                    return Ok(IpcResponse::wear_sampling(
                        WearSamplingStatus::AwaitingConsent,
                    ));
                }
                Ok(self.responses.lock().await.pop_front().unwrap())
            })
        }
    }

    fn test_state(ipc: Arc<dyn DaemonIpc>) -> WebState {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) = mpsc::channel(8);
        let (reload_tx, reload_rx) = broadcast::channel(8);
        let config = Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: KeymapConfig::default(),
            input_filter: InputFilterConfig::default(),
            publish: PublishConfig::default(),
        });
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(Arc::new(Credentials::default()));
        std::mem::forget((reload_tx, config_tx, creds_tx));
        let doctor = DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());
        WebState::new(WebStateInner::new_for_test_with_ipc(
            WebStateInnerParams {
                ctl_tx,
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path: PathBuf::from("/dev/null"),
                creds_path: PathBuf::from("/dev/null"),
                doctor,
                wear: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
                cancel: tokio_util::sync::CancellationToken::new(),
                reload_timeout: std::time::Duration::from_secs(1),
            },
            ipc,
        ))
    }

    #[test]
    fn response_status_is_the_only_serialized_shape() {
        let body = serde_json::to_value(WearSamplingStatus::Error(
            "wear_sampling_needs_consent".to_owned(),
        ))
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "status": "error",
                "reason": "wear_sampling_needs_consent",
            })
        );
        assert!(body.get("token").is_none());
        assert!(body.get("record").is_none());
        assert!(body.get("persistent_id").is_none());
    }

    #[test]
    fn status_extracts_only_wear_sampling_ipc_payload() {
        let response = IpcResponse::wear_sampling(WearSamplingStatus::Granted);
        assert_eq!(status(response).unwrap(), WearSamplingStatus::Granted);
    }

    #[tokio::test]
    async fn routes_forward_enable_poll_and_disable_without_consent_data() {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([
                IpcResponse::wear_sampling(WearSamplingStatus::TimedOut),
                IpcResponse::wear_sampling(WearSamplingStatus::Granted),
            ])),
            enable_started: Notify::new(),
            hold_enable: false,
        });
        let state = test_state(fake.clone());

        let (code, Json(enable)) = post_enable(State(state.clone())).await.unwrap();
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(enable, WearSamplingStatus::AwaitingConsent);
        tokio::task::yield_now().await;

        let Json(poll) = get_status(State(state.clone())).await.unwrap();
        assert_eq!(poll, WearSamplingStatus::TimedOut);
        let Json(disable) = post_disable(State(state), Json(DisableRequest { forget: true }))
            .await
            .unwrap();
        assert_eq!(disable, WearSamplingStatus::Granted);
        assert_eq!(
            *fake.requests.lock().await,
            vec![
                IpcRequest::WearSamplingEnable,
                IpcRequest::WearSamplingStatus,
                IpcRequest::WearSamplingDisable { forget: true },
            ]
        );
    }

    #[tokio::test]
    async fn duplicate_enable_returns_the_exact_single_flight_status_body() {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
            enable_started: Notify::new(),
            hold_enable: true,
        });
        let state = test_state(fake.clone());
        let _ = post_enable(State(state.clone())).await.unwrap();
        fake.enable_started.notified().await;
        let (code, Json(body)) = post_enable(State(state)).await.unwrap();
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(
            body,
            WearSamplingStatus::Error("wear_sampling_in_progress".to_owned())
        );
        assert_eq!(
            *fake.requests.lock().await,
            vec![IpcRequest::WearSamplingEnable]
        );
    }
}
