//! Token-free HTTP forwarding for active wear-sampling consent flows.
//!
//! Hosts the IPC-forwarding handlers (enable / poll / disable) plus the
//! onboarding-nudge dismiss handler (`POST /api/wear/sampling/nudge/dismiss`,
//! issue #186). The dismiss handler persists a flag file beside the
//! star-nudge persistence path so the operator sees a coherent cluster of
//! UI-state files in the config directory.
//!
//! Per-display forwarding (issue #185): every endpoint takes an optional
//! `?display=<id>` query parameter. When present, the request is routed to
//! the matching `WearSamplingEnableFor`/`StatusFor`/`DisableFor` IPC
//! variant — that is the only path that reaches a specific display under
//! multi-display configuration. Omission preserves the legacy unit-variant
//! wire tag, which the daemon only honors for exactly one selected display
//! (the daemon returns `E_CONFIG_INVALID: multiple displays configured` if
//! more than one is selected).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use dormant_core::ipc_proto::{IpcRequest, IpcResponse, WearSamplingStatus};

use crate::WebState;
use crate::error::WebError;
use crate::request_daemon_ipc;
use crate::routes::dismiss_flag::write_dismiss_flag;

/// Optional `?display=<id>` query parameter carried by every
/// wear-sampling endpoint. Issue #185: the per-display `*For` IPC variants
/// take an explicit display id; this query parameter is how the web UI
/// picks one. Absence is the legacy ergonomics path.
#[derive(serde::Deserialize, Default)]
pub(crate) struct DisplayQuery {
    /// Configured display id, when present.
    pub(crate) display: Option<String>,
}

fn status(response: IpcResponse) -> Result<WearSamplingStatus, WebError> {
    response
        .wear_sampling
        .ok_or(WebError::CoordinationUnavailable)
}

/// `POST /api/wear/sampling/enable?display=<id>` — start consent without
/// waiting for its portal window. The optional `display` query parameter
/// routes to `WearSamplingEnableFor { display }` when present; absence
/// preserves the legacy unit variant (single-display case).
pub(crate) async fn post_enable(
    State(state): State<WebState>,
    Query(query): Query<DisplayQuery>,
) -> Result<(StatusCode, Json<WearSamplingStatus>), WebError> {
    let Ok(guard) = std::sync::Arc::clone(&state.inner.wear_sampling_lock).try_lock_owned() else {
        return Ok((
            StatusCode::CONFLICT,
            Json(WearSamplingStatus::Error(
                "wear_sampling_in_progress".to_owned(),
            )),
        ));
    };
    let request = match query.display.as_deref() {
        Some(display) => IpcRequest::WearSamplingEnableFor {
            display: display.to_owned(),
        },
        None => IpcRequest::WearSamplingEnable,
    };
    tokio::spawn(async move {
        let _guard = guard;
        let _ = request_daemon_ipc(&state, request).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(WearSamplingStatus::AwaitingConsent),
    ))
}

/// `GET /api/wear/sampling?display=<id>` — poll the daemon-owned consent
/// status. The optional `display` query parameter routes to
/// `WearSamplingStatusFor { display }` when present; absence preserves the
/// legacy unit variant (single-display case).
pub(crate) async fn get_status(
    State(state): State<WebState>,
    Query(query): Query<DisplayQuery>,
) -> Result<Json<WearSamplingStatus>, WebError> {
    let request = match query.display.as_deref() {
        Some(display) => IpcRequest::WearSamplingStatusFor {
            display: display.to_owned(),
        },
        None => IpcRequest::WearSamplingStatus,
    };
    let response = request_daemon_ipc(&state, request).await?;
    Ok(Json(status(response)?))
}

/// `POST /api/wear/sampling/disable?display=<id>` — close sampling,
/// optionally forgetting consent. The optional `display` query parameter
/// routes to `WearSamplingDisableFor { display, forget }` when present;
/// absence preserves the legacy unit variant (single-display case).
pub(crate) async fn post_disable(
    State(state): State<WebState>,
    Query(query): Query<DisplayQuery>,
    Json(request): Json<DisableRequest>,
) -> Result<Json<WearSamplingStatus>, WebError> {
    let ipc_request = match query.display.as_deref() {
        Some(display) => IpcRequest::WearSamplingDisableFor {
            display: display.to_owned(),
            forget: request.forget,
        },
        None => IpcRequest::WearSamplingDisable {
            forget: request.forget,
        },
    };
    let response = request_daemon_ipc(&state, ipc_request).await?;
    Ok(Json(status(response)?))
}

#[derive(serde::Deserialize)]
pub(crate) struct DisableRequest {
    pub(crate) forget: bool,
}

/// `POST /api/wear/sampling/nudge/dismiss` — write the
/// `wear-sampling-nudge-dismissed` flag file so the wear-card onboarding
/// nudge (#186) never renders again. Idempotent. Same persistence
/// discipline as [`crate::routes::star_nudge::post_star_nudge_dismiss`],
/// shared via [`crate::routes::dismiss_flag::write_dismiss_flag`].
pub(crate) async fn post_wear_sampling_nudge_dismiss(
    State(state): State<WebState>,
) -> Result<impl IntoResponse, WebError> {
    let path = state.inner.wear_sampling_nudge_path.clone();
    if path.exists() {
        tracing::debug!(
            event = "wear_sampling_nudge_dismissed",
            ?path,
            "already dismissed"
        );
        return Ok(StatusCode::NO_CONTENT);
    }
    write_dismiss_flag(&path, "wear_sampling_nudge_dismissed")?;
    Ok(StatusCode::NO_CONTENT)
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
                wear_sampling_rx: tokio::sync::watch::channel(None).1,
                per_display_statuses_rx: tokio::sync::watch::channel(
                    std::collections::BTreeMap::new(),
                )
                .1,
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

        let (code, Json(enable)) =
            post_enable(State(state.clone()), Query(DisplayQuery::default()))
                .await
                .unwrap();
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(enable, WearSamplingStatus::AwaitingConsent);
        tokio::task::yield_now().await;

        let Json(poll) = get_status(State(state.clone()), Query(DisplayQuery::default()))
            .await
            .unwrap();
        assert_eq!(poll, WearSamplingStatus::TimedOut);
        let Json(disable) = post_disable(
            State(state),
            Query(DisplayQuery::default()),
            Json(DisableRequest { forget: true }),
        )
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
        let _ = post_enable(State(state.clone()), Query(DisplayQuery::default()))
            .await
            .unwrap();
        fake.enable_started.notified().await;
        let (code, Json(body)) = post_enable(State(state), Query(DisplayQuery::default()))
            .await
            .unwrap();
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

    // ── onboarding nudge dismiss endpoint ─────────────────────────────────
    //
    // Mirrors the star-nudge dismiss discipline (same atomic tempfile+rename
    // with `create_new(true)`, same idempotent path) — the only thing that
    // changes is the flag file name and the wire path. The handler is a
    // **future** addition; these tests are the RED tests that prove the
    // route is missing today.

    /// Build a `WebState` whose `wear_sampling_nudge_path` lives under
    /// `dir`, so the derived flag is `dir/wear-sampling-nudge-dismissed`.
    fn state_with_nudge_dir(dir: &std::path::Path) -> (WebState, std::path::PathBuf) {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
            enable_started: Notify::new(),
            hold_enable: false,
        });
        let mut state = test_state(fake);
        let flag_path = dir.join("wear-sampling-nudge-dismissed");
        assert!(!flag_path.exists(), "fresh tempdir must not carry the flag");
        // The `wear_sampling_nudge_path` field is the seam for the new
        // handler — this is the production constructor wiring the path
        // under the config dir, mirroring `star_nudge_path`. Mirrors the
        // `state.inner` mutation pattern in
        // `crate::routes::star_nudge::tests::post_star_route_*`.
        let inner =
            Arc::get_mut(&mut state.inner).expect("state must have a unique strong reference");
        inner.wear_sampling_nudge_path = flag_path.clone();
        (state, flag_path)
    }

    #[tokio::test]
    async fn nudge_dismiss_writes_flag_file_and_returns_204() {
        use axum::response::IntoResponse;
        let dir = tempfile::tempdir().unwrap();
        let (state, flag_path) = state_with_nudge_dir(dir.path());
        let result = post_wear_sampling_nudge_dismiss(State(state)).await;
        assert!(result.is_ok(), "dismiss handler must succeed");
        let response = result.unwrap().into_response();
        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "dismiss must return 204 No Content"
        );
        assert!(flag_path.exists(), "flag file must be created on disk");
        assert_eq!(
            std::fs::read_to_string(&flag_path).unwrap(),
            "dismissed\n",
            "flag file must carry the dismissed sentinel content"
        );
    }

    #[tokio::test]
    async fn nudge_dismiss_is_idempotent() {
        use axum::response::IntoResponse;
        let dir = tempfile::tempdir().unwrap();
        let (state, flag_path) = state_with_nudge_dir(dir.path());
        let first = post_wear_sampling_nudge_dismiss(State(state.clone()))
            .await
            .unwrap();
        assert_eq!(first.into_response().status(), StatusCode::NO_CONTENT);
        let mtime = flag_path.metadata().unwrap().modified().unwrap();
        let second = post_wear_sampling_nudge_dismiss(State(state))
            .await
            .unwrap();
        assert_eq!(second.into_response().status(), StatusCode::NO_CONTENT);
        assert_eq!(
            flag_path.metadata().unwrap().modified().unwrap(),
            mtime,
            "second dismiss must not rewrite the flag file"
        );
    }

    /// Pin the literal log event name emitted by
    /// `post_wear_sampling_nudge_dismiss`. The pre-extraction (`4e7381a`)
    /// literal was `wear_sampling_nudge_dismissed`; the extraction must
    /// preserve it byte-for-byte. A regression that derives the name from
    /// the filename (`wear_sampling_nudge_dismissed_dismissed`) would
    /// silently break dashboard greps and operator alerting.
    #[tokio::test]
    async fn nudge_dismiss_emits_wear_sampling_nudge_dismissed_event_literal() {
        use axum::response::IntoResponse;
        let dir = tempfile::tempdir().unwrap();
        let (state, _flag_path) = state_with_nudge_dir(dir.path());

        crate::test_support::start_capturing();
        let result = post_wear_sampling_nudge_dismiss(State(state)).await;
        assert!(result.is_ok());
        let _ = result.unwrap().into_response();
        let events = crate::test_support::take_captured();

        // Positive pin: the literal survives the extraction.
        assert!(
            events
                .iter()
                .any(|e| e.contains("event=\"wear_sampling_nudge_dismissed\"")),
            "missing literal event=wear_sampling_nudge_dismissed in captured events: {events:?}"
        );
        // Negative pin: the doubled-suffix regression would emit
        // `event="wear_sampling_nudge_dismissed_dismissed"` — guard
        // against it.
        assert!(
            !events
                .iter()
                .any(|e| e.contains("wear_sampling_nudge_dismissed_dismissed")),
            "do NOT derive the event name from the filename — silently emits the doubled suffix \
             and breaks grep-based monitoring; pass the literal at the call site instead"
        );
    }

    // ── #185 Task 24b — per-display HTTP forwarding ────────────────────
    //
    // Issue #185: every endpoint must accept `?display=<id>` and forward
    // it to the matching `*For` IPC variant. Omission preserves the
    // legacy unit variant so the existing single-display ergonomics
    // survive. The tests below pin the wire-traffic contract; the IPC
    // daemon-side validation of the display id is the daemon's job.

    /// `?display=<id>` on `enable` MUST reach `WearSamplingEnableFor`.
    /// Pin the exact display id the handler forwarded — a regression
    /// that drops the parameter (or rewrites it from `display_name` to
    /// some derived string) would silently re-route to a different
    /// sampler.
    #[tokio::test]
    async fn enable_forwards_display_query_to_wear_sampling_enable_for() {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
            enable_started: Notify::new(),
            hold_enable: false,
        });
        let state = test_state(fake.clone());
        let query = DisplayQuery {
            display: Some("desk".to_owned()),
        };
        let _ = post_enable(State(state.clone()), Query(query))
            .await
            .unwrap();
        // The `enable` handler spawns an IPC task; let it run.
        tokio::task::yield_now().await;
        let requests = fake.requests.lock().await.clone();
        assert_eq!(
            requests.len(),
            1,
            "expected one Enable request, got {requests:?}"
        );
        assert!(
            matches!(
                &requests[0],
                IpcRequest::WearSamplingEnableFor { display } if display == "desk"
            ),
            "enable must forward display=\"desk\" to WearSamplingEnableFor, got {:?}",
            requests[0]
        );
    }

    /// `?display=<id>` on `get_status` MUST reach `WearSamplingStatusFor`.
    #[tokio::test]
    async fn get_status_forwards_display_query_to_wear_sampling_status_for() {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([IpcResponse::wear_sampling(
                WearSamplingStatus::Granted,
            )])),
            enable_started: Notify::new(),
            hold_enable: false,
        });
        let state = test_state(fake.clone());
        let query = DisplayQuery {
            display: Some("tv".to_owned()),
        };
        let Json(body) = get_status(State(state), Query(query)).await.unwrap();
        assert_eq!(body, WearSamplingStatus::Granted);
        let requests = fake.requests.lock().await.clone();
        assert_eq!(requests.len(), 1);
        assert!(
            matches!(
                &requests[0],
                IpcRequest::WearSamplingStatusFor { display } if display == "tv"
            ),
            "status must forward display=\"tv\" to WearSamplingStatusFor, got {:?}",
            requests[0]
        );
    }

    /// `?display=<id>` on `disable` MUST reach `WearSamplingDisableFor`.
    #[tokio::test]
    async fn disable_forwards_display_query_to_wear_sampling_disable_for() {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([IpcResponse::wear_sampling(
                WearSamplingStatus::Granted,
            )])),
            enable_started: Notify::new(),
            hold_enable: false,
        });
        let state = test_state(fake.clone());
        let query = DisplayQuery {
            display: Some("desk".to_owned()),
        };
        let Json(body) = post_disable(
            State(state),
            Query(query),
            Json(DisableRequest { forget: true }),
        )
        .await
        .unwrap();
        assert_eq!(body, WearSamplingStatus::Granted);
        let requests = fake.requests.lock().await.clone();
        assert_eq!(requests.len(), 1);
        assert!(
            matches!(
                &requests[0],
                IpcRequest::WearSamplingDisableFor { display, forget } if display == "desk" && *forget
            ),
            "disable must forward display=\"desk\" + forget=true to WearSamplingDisableFor, got {:?}",
            requests[0]
        );
    }

    /// Omitting `?display` MUST preserve the legacy unit-variant wire
    /// tag on every endpoint (issue #185 backward compatibility).
    #[tokio::test]
    async fn omission_preserves_legacy_unit_variant_wire_tag() {
        let fake = Arc::new(FakeIpc {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([
                IpcResponse::wear_sampling(WearSamplingStatus::AwaitingConsent),
                IpcResponse::wear_sampling(WearSamplingStatus::Granted),
                IpcResponse::wear_sampling(WearSamplingStatus::Granted),
            ])),
            enable_started: Notify::new(),
            hold_enable: false,
        });
        let state = test_state(fake.clone());
        let _ = post_enable(State(state.clone()), Query(DisplayQuery::default()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let _ = get_status(State(state.clone()), Query(DisplayQuery::default()))
            .await
            .unwrap();
        let _ = post_disable(
            State(state),
            Query(DisplayQuery::default()),
            Json(DisableRequest { forget: false }),
        )
        .await
        .unwrap();
        let requests = fake.requests.lock().await.clone();
        assert_eq!(requests.len(), 3, "got {requests:?}");
        assert!(matches!(requests[0], IpcRequest::WearSamplingEnable));
        assert!(matches!(requests[1], IpcRequest::WearSamplingStatus));
        assert!(matches!(
            requests[2],
            IpcRequest::WearSamplingDisable { forget: false }
        ));
    }
}
