//! Instance pairing routes backed exclusively by the daemon IPC service.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use dormant_core::ipc_proto::{
    CoordinationPairOpenResponse, CoordinationPairStatus, CoordinationPeers, IpcRequest,
    IpcResponse,
};
use serde::{Deserialize, Serialize};

use crate::error::WebError;
use crate::state::WebState;

/// Request body for opening a local responder window.
#[derive(Debug, Deserialize)]
pub(crate) struct InstancePairRequest {
    /// Local display name included in the discovery announcement.
    pub(crate) display_name: String,
}

/// Explicit initiator confirmation request.
#[derive(Debug, Deserialize)]
pub(crate) struct InstancePairJoinRequest {
    pub(crate) display_name: String,
    pub(crate) instance_id: String,
    pub(crate) code: String,
}

/// Status DTO that deliberately excludes the one-time code.
#[derive(Debug, Serialize)]
pub(crate) struct InstancePairStatus {
    state: String,
    detail: Option<String>,
}

fn enabled(state: &WebState) -> Result<(), WebError> {
    if state.inner.config_rx.borrow().coordination.enabled {
        Ok(())
    } else {
        Err(WebError::CoordinationDisabled)
    }
}

async fn request(state: &WebState, request: IpcRequest) -> Result<IpcResponse, WebError> {
    let socket = dormant_core::paths::resolve_socket_path(
        state.inner.config_rx.borrow().daemon.socket_path.as_deref(),
    );
    tokio::task::spawn_blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)
            .map_err(|_| WebError::CoordinationUnavailable)?;
        let line =
            serde_json::to_string(&request).map_err(|_| WebError::CoordinationUnavailable)?;
        writeln!(stream, "{line}").map_err(|_| WebError::CoordinationUnavailable)?;
        stream
            .flush()
            .map_err(|_| WebError::CoordinationUnavailable)?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|_| WebError::CoordinationUnavailable)?;
        serde_json::from_str(&line).map_err(|_| WebError::CoordinationUnavailable)
    })
    .await
    .map_err(|_| WebError::CoordinationUnavailable)?
}

fn status(response: IpcResponse) -> Result<InstancePairStatus, WebError> {
    let CoordinationPairStatus {
        state,
        peer_instance_id,
        ..
    } = response
        .coordination_pair
        .ok_or(WebError::CoordinationUnavailable)?;
    Ok(InstancePairStatus {
        state,
        detail: peer_instance_id,
    })
}

/// Open a local pairing window and return its one-time code exactly once.
pub(crate) async fn post_pair_instance(
    State(state): State<WebState>,
    Json(body): Json<InstancePairRequest>,
) -> Result<(StatusCode, Json<CoordinationPairOpenResponse>), WebError> {
    enabled(&state)?;
    let Ok(guard) = Arc::clone(&state.inner.pair_lock).try_lock_owned() else {
        return Err(WebError::PairInProgress);
    };
    let response = request(
        &state,
        IpcRequest::CoordinationPairOpen {
            display_name: body.display_name,
        },
    )
    .await?;
    drop(guard);
    if response
        .error
        .as_deref()
        .is_some_and(|error| error == "a pairing session is already active")
    {
        return Err(WebError::PairInProgress);
    }
    let opened = response
        .coordination_pair_open
        .ok_or(WebError::CoordinationUnavailable)?;
    Ok((StatusCode::ACCEPTED, Json(opened)))
}

/// Return non-secret lifecycle state for a local responder window.
pub(crate) async fn get_pair_instance(
    State(state): State<WebState>,
    Path(pair_id): Path<String>,
) -> Result<Json<InstancePairStatus>, WebError> {
    enabled(&state)?;
    status(request(&state, IpcRequest::CoordinationPairStatus { pair_id }).await?).map(Json)
}

/// Submit an operator-confirmed code to a selected discovered instance.
pub(crate) async fn post_join_pair_instance(
    State(state): State<WebState>,
    Json(body): Json<InstancePairJoinRequest>,
) -> Result<(StatusCode, Json<InstancePairStatus>), WebError> {
    enabled(&state)?;
    let response = request(
        &state,
        IpcRequest::CoordinationPairJoin {
            display_name: body.display_name,
            instance_id: body.instance_id,
            code: body.code,
        },
    )
    .await?;
    if !response.ok {
        return Err(WebError::CoordinationUnavailable);
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(InstancePairStatus {
            state: "pairing".into(),
            detail: None,
        }),
    ))
}

/// Cancel a local responder window.
pub(crate) async fn post_cancel_pair_instance(
    State(state): State<WebState>,
    Path(pair_id): Path<String>,
) -> Result<(StatusCode, Json<InstancePairStatus>), WebError> {
    enabled(&state)?;
    let status = status(request(&state, IpcRequest::CoordinationPairCancel { pair_id }).await?)?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

/// Return the read-only public inventory used by the pairing UI.
pub(crate) async fn get_pair_instance_peers(
    State(state): State<WebState>,
) -> Result<Json<CoordinationPeers>, WebError> {
    enabled(&state)?;
    let response = request(&state, IpcRequest::CoordinationPeersList).await?;
    response
        .coordination_peers
        .map(Json)
        .ok_or(WebError::CoordinationUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use dormant_core::config::CoordinationConfig;
    use dormant_core::config::schema::{
        AudioConfig, Config, Credentials, DaemonConfig, NotificationsConfig, WatchdogConfig,
        WearConfig,
    };
    use dormant_core::reload::ReloadRequester;
    use dormant_core::rules::ControlMsg;
    use dormant_core::wear::WearHandle;
    use dormant_doctor::DoctorService;
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    fn state(enabled: bool) -> WebState {
        state_with_socket(enabled, None)
    }

    fn state_with_socket(enabled: bool, socket: Option<&std::path::Path>) -> WebState {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<ControlMsg>(8);
        let (reload_tx, reload_rx) = broadcast::channel(8);
        let (reload_request_tx, _reload_request_rx) = mpsc::channel(8);
        let config = Arc::new(Config {
            coordination: CoordinationConfig {
                enabled,
                ..Default::default()
            },
            config_version: 1,
            daemon: DaemonConfig {
                socket_path: socket.map(std::path::Path::to_path_buf),
                ..DaemonConfig::default()
            },
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        });
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(Arc::new(Credentials::default()));
        std::mem::forget((reload_tx, config_tx, creds_tx));
        let doctor = DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());
        WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx,
                reload_requester: ReloadRequester::new(reload_request_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path: PathBuf::from("/dev/null"),
                creds_path: PathBuf::from("/dev/null"),
                doctor,
                wear: WearHandle::default(),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
                cancel: CancellationToken::new(),
                reload_timeout: std::time::Duration::from_secs(1),
            },
        ))
    }

    async fn call(router: axum::Router, request: Request<Body>) -> axum::response::Response {
        router.oneshot(request).await.unwrap()
    }

    fn ipc_server(
        socket: &std::path::Path,
        responses: Vec<IpcResponse>,
        delay: std::time::Duration,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                std::io::BufRead::read_line(
                    &mut std::io::BufReader::new(stream.try_clone().unwrap()),
                    &mut request,
                )
                .unwrap();
                thread::sleep(delay);
                std::io::Write::write_all(
                    &mut stream,
                    format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes(),
                )
                .unwrap();
            }
        })
    }

    fn post_open() -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/api/pair/instance")
            .header("Host", "127.0.0.1:8080")
            .header("Origin", "http://127.0.0.1:8080")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"display_name":"Office"}"#))
            .unwrap()
    }

    #[tokio::test]
    async fn pair_instance_coordination_disabled_returns_403() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/pair/instance")
            .header("Host", "127.0.0.1:8080")
            .header("Origin", "http://127.0.0.1:8080")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"display_name":"Office"}"#))
            .unwrap();
        let response = call(crate::server::build_router(state(false)), request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body, r#"{"error":"coordination_disabled"}"#);
    }

    #[tokio::test]
    async fn pair_instance_cross_and_missing_origin_are_rejected() {
        for origin in [Some("http://evil.invalid"), None] {
            let mut builder = Request::builder()
                .method(Method::POST)
                .uri("/api/pair/instance")
                .header("Host", "127.0.0.1:8080")
                .header("Content-Type", "application/json");
            if let Some(origin) = origin {
                builder = builder.header("Origin", origin);
            }
            let response = call(
                crate::server::build_router(state(true)),
                builder
                    .body(Body::from(r#"{"display_name":"Office"}"#))
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn pair_instance_cancel_missing_and_foreign_origin_are_rejected() {
        for origin in [Some("http://evil.invalid"), None] {
            let mut builder = Request::builder()
                .method(Method::POST)
                .uri("/api/pair/instance/pair-1/cancel")
                .header("Host", "127.0.0.1:8080")
                .header("Content-Type", "application/json");
            if let Some(origin) = origin {
                builder = builder.header("Origin", origin);
            }
            let response = call(
                crate::server::build_router(state(true)),
                builder.body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn pair_instance_body_over_4kib_returns_413() {
        let response = call(
            crate::server::build_router(state(true)),
            Request::builder()
                .method(Method::POST)
                .uri("/api/pair/instance")
                .header("Host", "127.0.0.1:8080")
                .header("Origin", "http://127.0.0.1:8080")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(
                    r#"{{"display_name":"{}"}}"#,
                    "x".repeat(4097)
                )))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn pair_instance_second_attempt_returns_409() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ipc.sock");
        let server = ipc_server(
            &socket,
            vec![
                IpcResponse::coordination_pair_open(CoordinationPairOpenResponse {
                    pair_id: "pair-1".into(),
                    code: "ABCD1234".into(),
                    expires_at: "soon".into(),
                }),
                IpcResponse::error("a pairing session is already active"),
            ],
            std::time::Duration::ZERO,
        );
        let router = crate::server::build_router(state_with_socket(true, Some(&socket)));
        let first = call(router.clone(), post_open()).await;
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let second = call(router, post_open()).await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn pair_instance_cancel_route_maps_ipc_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ipc.sock");
        let cancelled = dormant_core::ipc_proto::CoordinationPairStatus {
            pair_id: "pair-1".into(),
            state: "cancelled".into(),
            peer_instance_id: None,
        };
        let server = ipc_server(
            &socket,
            vec![
                IpcResponse::coordination_pair_open(CoordinationPairOpenResponse {
                    pair_id: "pair-1".into(),
                    code: "ABCD1234".into(),
                    expires_at: "soon".into(),
                }),
                IpcResponse::coordination_pair(cancelled.clone()),
                IpcResponse::coordination_pair(cancelled),
            ],
            std::time::Duration::ZERO,
        );
        let router = crate::server::build_router(state_with_socket(true, Some(&socket)));
        assert_eq!(
            call(router.clone(), post_open()).await.status(),
            StatusCode::ACCEPTED
        );
        let cancel = Request::builder()
            .method(Method::POST)
            .uri("/api/pair/instance/pair-1/cancel")
            .header("Host", "127.0.0.1:8080")
            .header("Origin", "http://127.0.0.1:8080")
            .header("Content-Type", "application/json")
            .body(Body::empty())
            .unwrap();
        let cancelled = call(router.clone(), cancel).await;
        assert_eq!(cancelled.status(), StatusCode::ACCEPTED);
        let status = call(
            router,
            Request::builder()
                .uri("/api/pair/instance/pair-1")
                .header("Host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status.status(), StatusCode::OK);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn pair_instance_status_route_maps_timeout_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ipc.sock");
        let server = ipc_server(
            &socket,
            vec![IpcResponse::coordination_pair(
                dormant_core::ipc_proto::CoordinationPairStatus {
                    pair_id: "expired".into(),
                    state: "timeout".into(),
                    peer_instance_id: None,
                },
            )],
            std::time::Duration::ZERO,
        );
        let router = crate::server::build_router(state_with_socket(true, Some(&socket)));
        let response = call(
            router,
            Request::builder()
                .uri("/api/pair/instance/expired")
                .header("Host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("timeout"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn pair_instance_response_bodies_are_code_free_except_open() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ipc.sock");
        let secret = "SENTINEL-CODE";
        let status = dormant_core::ipc_proto::CoordinationPairStatus {
            pair_id: "pair-1".into(),
            state: "cancelled".into(),
            peer_instance_id: None,
        };
        let server = ipc_server(
            &socket,
            vec![
                IpcResponse::coordination_pair_open(CoordinationPairOpenResponse {
                    pair_id: "pair-1".into(),
                    code: secret.into(),
                    expires_at: "soon".into(),
                }),
                IpcResponse::coordination_pair(status.clone()),
                IpcResponse::coordination_pair(status),
                IpcResponse::coordination_peers(CoordinationPeers {
                    discovered: vec![],
                    paired: vec![],
                }),
            ],
            std::time::Duration::ZERO,
        );
        let router = crate::server::build_router(state_with_socket(true, Some(&socket)));
        let open = axum::body::to_bytes(
            call(router.clone(), post_open()).await.into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        assert!(std::str::from_utf8(&open).unwrap().contains(secret));
        for request in [
            Request::builder()
                .uri("/api/pair/instance/pair-1")
                .header("Host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/pair/instance/pair-1/cancel")
                .header("Host", "127.0.0.1:8080")
                .header("Origin", "http://127.0.0.1:8080")
                .header("Content-Type", "application/json")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/api/pair/instance/peers")
                .header("Host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        ] {
            let body =
                axum::body::to_bytes(call(router.clone(), request).await.into_body(), usize::MAX)
                    .await
                    .unwrap();
            assert!(!std::str::from_utf8(&body).unwrap().contains(secret));
        }
        server.join().unwrap();
    }
}
