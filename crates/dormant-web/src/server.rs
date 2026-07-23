//! Axum HTTP server — router + listener lifecycle.
//!
//! Builds a [`Router`] on the caller-supplied [`WebState`] (see
//! [`crate::WebState`]), binds a TCP listener, and serves with graceful
//! shutdown wired to the [`CancellationToken`] in the state.

use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use dormant_core::rules::{ControlMsg, StateSnapshot};
use tokio::sync::oneshot;

use crate::WebState;
use crate::assets;
use crate::error::WebError;
use crate::routes::{
    command, config, config_apply, daemon, doctor, events, operations, pair, pair_dormant, wear,
};
use crate::security::security_guard;

/// Duration the `/api/state` handler waits for a snapshot reply before
/// returning 504.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Full `/api`-prefixed `POST` routes registered via [`route_post!`],
/// derived from actual router construction rather than hand-maintained —
/// see the inverted meta-test in the `tests` module below, which asserts
/// every entry here is classified in `security::STRICT_ORIGIN_PATHS` or
/// `security::ACKNOWLEDGED_WEAK_ROUTES`.
///
/// `OnceLock<Mutex<Vec<_>>>` rather than a bare `OnceLock<Vec<_>>`: the
/// latter is write-once and cannot accumulate more than the first
/// registration. The `Mutex` gives interior mutability so each
/// `route_post!` invocation can push onto the same list.
static REGISTERED_POST_ROUTES: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();

/// Record `path` (already `/api`-prefixed) in [`REGISTERED_POST_ROUTES`]
/// if it isn't already present.
///
/// `build_router` runs more than once in this crate (each test that
/// builds a router calls it again), so this must be idempotent —
/// push-if-absent rather than assume single registration — or repeated
/// test runs would silently accumulate duplicates.
fn register_post_route(path: &'static str) {
    let routes = REGISTERED_POST_ROUTES.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !guard.contains(&path) {
        guard.push(path);
    }
}

/// Snapshot of every full `/api`-prefixed `POST` route registered via
/// [`route_post!`] so far, across every `build_router` call made in this
/// process.
///
/// Test-only introspection: it exists to let the inverted route
/// meta-test (below) read back what `route_post!` derived from actual
/// registration, rather than a hand-maintained list.
#[cfg(test)]
fn registered_post_routes() -> Vec<&'static str> {
    let routes = REGISTERED_POST_ROUTES.get_or_init(|| Mutex::new(Vec::new()));
    routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Mounts a `POST` route on `$router` at `$path` (relative to the `/api`
/// nest prefix used by `build_router`) and records the FULL
/// `/api`-prefixed path in [`REGISTERED_POST_ROUTES`], so the inverted
/// route meta-test can derive the live write-route set from actual
/// registration instead of a hand-maintained list.
///
/// `$path` must be a string literal: `concat!` needs it at compile time
/// to build the `&'static str` pushed into the registry with no
/// allocation. `$method_router` is passed through untouched, so a caller
/// can still layer route-specific middleware (e.g.
/// `post(handler).layer(DefaultBodyLimit::max(..))`) before handing it
/// to this macro — the macro only wraps `.route()`, it never wraps the
/// method router itself, so that composition keeps working unchanged.
macro_rules! route_post {
    ($router:expr, $path:literal, $method_router:expr) => {{
        register_post_route(concat!("/api", $path));
        $router.route($path, $method_router)
    }};
}

/// Build the axum [`Router`] on the given state, mounting all HTTP routes
/// behind the Host/Origin security guard.  API routes are nested under
/// `/api`; all other paths are served by the SPA fallback (embedded
/// `webui/dist`).
pub(crate) fn build_router(state: WebState) -> Router {
    let api = Router::new()
        .route("/state", get(get_state))
        .route("/config", get(config::get_config));
    let api = route_post!(
        api,
        "/config/apply",
        post(config_apply::post_apply).layer(DefaultBodyLimit::max(64 * 1024))
    );
    let api = route_post!(
        api,
        "/pair/instance",
        post(pair_dormant::post_pair_instance).layer(DefaultBodyLimit::max(4 * 1024))
    );
    let api = route_post!(
        api,
        "/pair/instance/join",
        post(pair_dormant::post_join_pair_instance).layer(DefaultBodyLimit::max(4 * 1024))
    );
    let api = route_post!(
        api,
        "/pair/instance/:id/cancel",
        post(pair_dormant::post_cancel_pair_instance).layer(DefaultBodyLimit::max(4 * 1024))
    );
    let api = route_post!(api, "/blank", post(command::post_blank));
    let api = route_post!(api, "/wake", post(command::post_wake));
    let api = route_post!(api, "/pause", post(command::post_pause));
    let api = route_post!(api, "/resume", post(command::post_resume));
    let api = route_post!(api, "/reload", post(command::post_reload));
    let api = route_post!(api, "/doctor", post(doctor::post_doctor));
    let api = route_post!(api, "/emergency-wake", post(command::post_emergency_wake));
    let api = route_post!(
        api,
        "/pair/samsung",
        post(pair::post_pair_samsung).layer(DefaultBodyLimit::max(4 * 1024))
    );
    let api = route_post!(
        api,
        "/doctor/exercise/:display",
        post(doctor::post_exercise)
    );
    let api = api
        .route("/events", get(events::ws_events))
        .route("/operations", get(operations::get_operations))
        .route("/daemon", get(daemon::get_daemon))
        .route("/wear", get(wear::get_wear))
        .route("/wear/:display", get(wear::get_wear_detail))
        .route("/pair/samsung/:id", get(pair::get_pair_samsung))
        .route(
            "/pair/instance/peers",
            get(pair_dormant::get_pair_instance_peers),
        )
        .route("/pair/instance/:id", get(pair_dormant::get_pair_instance))
        // API miss → 404, never the SPA fallback.
        .fallback(api_not_found)
        .with_state(state.clone());

    Router::new()
        .nest("/api", api)
        .fallback(assets::spa_fallback)
        // Security guard on ALL routes, including the SPA fallback.
        .layer(from_fn_with_state(state.clone(), security_guard))
        .with_state(state)
}

/// Bind, report the resolved address via `addr_tx`, and serve until the
/// [`CancellationToken`] fires.  Called from the spawned server task.
pub(crate) async fn serve_and_report(
    bind: SocketAddr,
    state: WebState,
    addr_tx: oneshot::Sender<SocketAddr>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    // The receiver may have already been dropped (e.g. if spawn returned
    // early due to a timeout) — ignore the send error.
    let _ = addr_tx.send(local_addr);

    let cancel = state.inner.cancel.clone();
    let router = build_router(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await?;

    Ok(())
}

/// API fallback — return 404 for unmatched `/api/*` paths so they are
/// never served the SPA `index.html`.
async fn api_not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found")
}

/// `GET /api/state` — send a [`ControlMsg::Snapshot`] through the
/// control channel, await the reply, and return it as JSON.
async fn get_state(State(state): State<WebState>) -> Result<Json<StateSnapshot>, WebError> {
    let (tx, rx) = oneshot::channel();

    state
        .inner
        .ctl_tx
        .send(ControlMsg::Snapshot(tx))
        .await
        .map_err(|_| WebError::EngineUnavailable)?;

    let snapshot = tokio::time::timeout(SNAPSHOT_TIMEOUT, rx)
        .await
        .map_err(|_| WebError::SnapshotTimeout)?
        .map_err(|_| WebError::SnapshotCancelled)?;

    Ok(Json(snapshot))
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, header};
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use tower::util::ServiceExt;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::{
        ControlMsg, DisplaySnapshot, EmergencyWakeReport, EmergencyWakeResult, ExerciseReport,
        StateSnapshot,
    };
    use dormant_core::types::DisplayId;
    use dormant_doctor::DoctorService;

    use crate::WebState;
    use crate::state::{WebStateInner, WebStateInnerParams};

    /// Build a minimal [`WebState`] suitable for testing `build_router`.
    /// The API routes will fail if called (no real engine behind the
    /// channels), but the security guard and SPA fallback work
    /// independently of the engine.
    fn test_web_state_with_bind(
        bind: SocketAddr,
    ) -> (
        WebState,
        CancellationToken,
        tokio::sync::mpsc::Receiver<ControlMsg>,
    ) {
        let cancel = CancellationToken::new();

        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::channel::<ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) =
            tokio::sync::mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
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

        let (config_tx, config_rx) = tokio::sync::watch::channel(config);
        let (creds_tx, creds_rx) = tokio::sync::watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor = DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let state = WebState::new(WebStateInner::new_for_test(WebStateInnerParams {
            ctl_tx: ctl_tx.clone(),
            reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            creds_path: std::path::PathBuf::from("/dev/null"),
            doctor,
            wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            web_bind: bind,
            cancel: cancel.clone(),
            reload_timeout: Duration::from_secs(10),
        }));

        (state, cancel, ctl_rx)
    }

    // ── Security guard covers static/SPA paths ────────────────────────────

    #[tokio::test]
    async fn security_guard_rejects_foreign_host_on_root() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);
        let router = build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "GET / with foreign Host must be rejected (403)"
        );
    }

    #[tokio::test]
    async fn security_guard_rejects_foreign_host_on_spa_route() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);
        let router = build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/some/spa/route")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "SPA route with foreign Host must be rejected (403)"
        );
    }

    #[tokio::test]
    async fn security_guard_allows_loopback_host_on_root() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);
        let router = build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("Host", "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "GET / with loopback Host must succeed"
        );

        // Sanity: the response should be the SPA index (text/html).
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        assert_eq!(
            ct,
            Some("text/html"),
            "root with legit Host should serve text/html"
        );
    }

    // ── Inverted route meta-test (Task 3, P2) ──────────────────────────────

    /// The derived write-route list must be non-empty and must contain
    /// every known `POST` route mounted by `build_router` — otherwise an
    /// empty (or partial) derivation would make the `⊆ STRICT ∪ WEAK`
    /// check below vacuously true instead of actually exercising anything.
    #[tokio::test]
    async fn derived_post_routes_cover_all_known_post_mounts() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);
        // Building the router runs every route_post! call site, populating
        // the derived registry.
        let _router = build_router(state);

        let derived = registered_post_routes();
        assert!(
            !derived.is_empty(),
            "no POST routes were registered via route_post! — the meta-test \
             below would vacuously pass with nothing to check"
        );

        let known_post_routes = [
            "/api/config/apply",
            "/api/blank",
            "/api/wake",
            "/api/pause",
            "/api/resume",
            "/api/reload",
            "/api/doctor",
            "/api/emergency-wake",
            "/api/doctor/exercise/:display",
        ];
        for known in known_post_routes {
            assert!(
                derived.contains(&known),
                "{known} is mounted with .route() but missing from the \
                 route_post! derivation — a forgotten route_post! conversion"
            );
        }
    }

    /// The INVERTED meta-test: every derived `POST` route must be
    /// classified in `STRICT_ORIGIN_PATHS` or `ACKNOWLEDGED_WEAK_ROUTES`.
    /// A route registered via `route_post!` but classified in neither is
    /// RED — a forgotten origin-strictness decision can no longer stay
    /// green just because nobody remembered to hand-copy it into a list.
    #[tokio::test]
    async fn derived_post_routes_are_classified_strict_or_weak() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);
        let _router = build_router(state);

        let derived = registered_post_routes();
        assert!(!derived.is_empty(), "derivation must not be empty");

        for path in &derived {
            assert!(
                crate::security::STRICT_ORIGIN_PATHS.contains(path)
                    || crate::security::ACKNOWLEDGED_WEAK_ROUTES.contains(path),
                "{path} is a registered POST route but is classified in \
                 neither STRICT_ORIGIN_PATHS nor ACKNOWLEDGED_WEAK_ROUTES — \
                 every write route must make an explicit origin-strictness \
                 decision"
            );
        }
    }

    // ── Emergency-wake route (Task 2) ──────────────────────────────────────

    /// Exercises the endpoint through the real `build_router` (not the
    /// `command_test_router` test mount) — full production wiring: engine
    /// channel, security guard, JSON body extraction, wire-shape response.
    #[tokio::test]
    async fn build_router_mounts_emergency_wake_and_returns_wire_report() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, mut ctl_rx) = test_web_state_with_bind(bind);
        tokio::spawn(async move {
            let Some(ControlMsg::EmergencyWake { reply }) = ctl_rx.recv().await else {
                panic!("expected EmergencyWake");
            };
            let _ = reply.send(EmergencyWakeReport {
                paused: true,
                displays: vec![EmergencyWakeResult {
                    display: DisplayId("studio".to_string()),
                    ok: true,
                    error: None,
                }],
            });
        });

        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/emergency-wake")
                    .header("Host", "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
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
                "paused": true,
                "displays": [{"display": "studio", "ok": true}]
            })
        );
    }

    // ── Operations guard-status route (Task 3) ──────────────────────────────

    #[tokio::test]
    async fn build_router_operations_reflects_web_guard_lifetimes() {
        let (state, _cancel, _ctl_rx) =
            test_web_state_with_bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0));
        let emergency_guard = state.inner.emergency_wake_lock.clone().lock_owned().await;
        state
            .inner
            .exercise_in_flight
            .lock()
            .await
            .insert(DisplayId("studio".to_string()));
        let router = build_router(state.clone());

        let held = router
            .clone()
            .oneshot(
                Request::get("/api/operations")
                    .header(axum::http::header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(held.status(), StatusCode::OK);
        assert_eq!(
            held.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
        );
        let held: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(held.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            held,
            serde_json::json!({
                "exercise_in_flight": ["studio"],
                "emergency_wake_in_flight": true
            })
        );

        drop(emergency_guard);
        state.inner.exercise_in_flight.lock().await.clear();
        let released = router
            .oneshot(
                Request::get("/api/operations")
                    .header(axum::http::header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(released.status(), StatusCode::OK);
        assert_eq!(
            released
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
        );
        let released: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(released.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            released,
            serde_json::json!({
                "exercise_in_flight": [],
                "emergency_wake_in_flight": false
            })
        );
    }

    /// Exercises `POST /api/doctor/exercise/:display` through the real
    /// `build_router` (not a bare test mount) — full production wiring:
    /// engine channel, security guard, dynamic path segment, wire-shape
    /// response.
    #[tokio::test]
    async fn build_router_mounts_display_exercise_and_returns_wire_report() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, mut ctl_rx) = test_web_state_with_bind(bind);
        tokio::spawn(async move {
            while let Some(message) = ctl_rx.recv().await {
                match message {
                    ControlMsg::Snapshot(reply) => {
                        let _ = reply.send(StateSnapshot {
                            sensors: Vec::new(),
                            zones: Vec::new(),
                            displays: vec![(
                                "main".to_string(),
                                DisplaySnapshot {
                                    phase: "active".to_string(),
                                    inhibited: false,
                                    paused: false,
                                    cmd_gen: 1,
                                    scope: dormant_core::config::DisplayScope::Private,
                                    owned: true,
                                    observed_input_code: None,
                                    panel_state: None,
                                    controllers: Vec::new(),
                                    wake_attempts: 0,
                                    last_blank_failed: false,
                                    stage: None,
                                },
                            )],
                            pending_reload: None,
                            rollback: None,
                        });
                    }
                    ControlMsg::Exercise { display, reply } => {
                        let _ = reply.send(ExerciseReport {
                            display,
                            pre_phase: "active".to_string(),
                            paused_rules: Vec::new(),
                            steps: Vec::new(),
                        });
                        break;
                    }
                    other => panic!("unexpected route message: {other:?}"),
                }
            }
        });

        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/doctor/exercise/main")
                    .header("Host", "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
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
                "steps": []
            })
        );
    }

    // ── Daemon identity route ────────────────────────────────────────────────

    /// Exercises `GET /api/daemon` through the real `build_router` (not a
    /// bare test mount) — full production wiring: security guard,
    /// `Host`-header requirement, no-store header, wire shape.
    #[tokio::test]
    async fn build_router_daemon_returns_identity_with_no_store() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);

        let response = build_router(state)
            .oneshot(
                Request::get("/api/daemon")
                    .header(axum::http::header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
        );
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["pid"], serde_json::json!(std::process::id()));
        assert_eq!(
            body["version"],
            serde_json::json!(env!("CARGO_PKG_VERSION"))
        );
        assert!(body["started_epoch_s"].as_u64().unwrap() > 0);
        assert!(body["socket"].as_str().unwrap().ends_with("dormant.sock"));
    }

    /// `GET /api/daemon` with a foreign `Host` header is rejected — the
    /// security guard applies to every route, including new GET-only ones
    /// that need no `STRICT_ORIGIN_PATHS`/`ACKNOWLEDGED_WEAK_ROUTES`
    /// classification (that registry is POST-only; see `route_post!`).
    #[tokio::test]
    async fn build_router_daemon_rejects_foreign_host() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel, _ctl_rx) = test_web_state_with_bind(bind);

        let response = build_router(state)
            .oneshot(
                Request::get("/api/daemon")
                    .header(axum::http::header::HOST, "evil.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
