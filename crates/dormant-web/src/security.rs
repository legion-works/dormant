//! Security guard — Host-header allow-list, CSRF defence, CORS hardening.
//!
//! Every request passes through the Host check unconditionally (anti
//! DNS-rebind).  State-changing methods (`POST`) additionally require
//! `Content-Type: application/json` and a same-origin disposition.
//! WebSocket upgrade requests (`GET` with `Upgrade: websocket`) also
//! pass the same-origin Origin check to prevent Cross-Site WebSocket
//! Hijacking (CSWSH).
//!
//! ## Design invariants
//!
//! - Never emit `Access-Control-Allow-Origin: *`.
//! - Literal event names for structured logging: `web_reject_host`,
//!   `web_reject_origin`.

use std::net::IpAddr;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::WebState;

/// Allowed loopback host values.  Both bracketed and bare IPv6 forms are
/// included because the Host header may carry either (`[::1]` or `[::1]:8080`).
const ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1", "[::1]"];

/// Full `/api`-prefixed `POST` routes that require the *strict* Origin
/// check (`check_apply_origin`): the Origin header MUST be present, MUST
/// be loopback, and MUST match the bound port exactly.
///
/// `/api/pair/samsung` is not mounted yet (a later task adds it), but it
/// is listed here now on purpose: the inverted route meta-test in
/// `server.rs` derives the live set of registered `POST` routes from
/// actual router construction and asserts every entry is classified in
/// either this set or [`ACKNOWLEDGED_WEAK_ROUTES`].  Pre-registering the
/// strict classification here means the moment `/api/pair/samsung` is
/// mounted, it is already strict by construction — there is no window
/// where a forgotten classification decision defaults to the weaker
/// same-origin check.
pub(crate) static STRICT_ORIGIN_PATHS: &[&str] = &[
    "/api/config/apply",
    "/api/pair/samsung",
    "/api/pair/instance",
    "/api/pair/instance/join",
    "/api/pair/instance/:id/cancel",
];

/// Full `/api`-prefixed `POST` routes that are deliberately left on the
/// generic same-origin check (`is_same_origin`) rather than the strict
/// exact-port check in [`STRICT_ORIGIN_PATHS`].
///
/// This is an *acknowledgement*, not a default: the inverted route
/// meta-test in `server.rs` asserts every `POST` route registered via
/// `route_post!` appears in this set or `STRICT_ORIGIN_PATHS`. Adding a
/// new route here (instead of to `STRICT_ORIGIN_PATHS`) is a conscious,
/// reviewable security decision that a route does not need the stricter
/// check, not an oversight that a route was never classified at all.
///
/// Not read by any runtime request path (only `STRICT_ORIGIN_PATHS` is —
/// membership here just means "not strict", the generic `is_same_origin`
/// check still applies to every POST route). Consumed only by the
/// inverted route meta-test in `server.rs` and this module's own tests,
/// hence `#[allow(dead_code)]` on non-test builds.
#[allow(dead_code)]
pub(crate) const ACKNOWLEDGED_WEAK_ROUTES: &[&str] = &[
    "/api/blank",
    "/api/wake",
    "/api/pause",
    "/api/resume",
    "/api/reload",
    "/api/doctor",
    "/api/emergency-wake",
    "/api/doctor/exercise/:display",
];

/// Reject any request whose `Host` header is not in the allow-list.
///
/// Returns 403 with literal event name `web_reject_host`.
pub(crate) async fn security_guard(
    State(state): State<WebState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    // ── Host-header allow-list ─────────────────────────────────────────────
    let host_ok = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| {
            let host_lower = h.to_lowercase();
            let host_only = extract_host_without_port(&host_lower);
            ALLOWED_HOSTS.contains(&host_only.as_str())
                || host_only == state.inner.web_bind.ip().to_string()
        });

    if !host_ok {
        tracing::warn!(
            event = "web_reject_host",
            host = ?headers.get(axum::http::header::HOST),
        );
        let body = serde_json::json!({ "error": "web_reject_host" });
        return (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
    }

    // ── POST security: JSON-only + same-origin ─────────────────────────────
    if request.method() == axum::http::Method::POST {
        // Require Content-Type: application/json for state-changing routes.
        let content_type_ok = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json"));

        if !content_type_ok {
            tracing::warn!(event = "web_reject_origin", reason = "content_type");
            let body = serde_json::json!({ "error": "web_reject_origin" });
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, axum::Json(body)).into_response();
        }

        // For strict-origin write endpoints, the Origin header MUST be
        // present (the generic is_same_origin allows absent Origin, which is
        // a CSRF gap for a write endpoint) and must exact-match the loopback
        // origin including the actual bound port. Patterns are matched segment
        // by segment so parameterized routes cannot silently fall back to the
        // weaker absent-Origin policy.
        if STRICT_ORIGIN_PATHS
            .iter()
            .any(|pattern| route_pattern_matches(pattern, request.uri().path()))
        {
            let origin_ok = check_apply_origin(&headers, state.inner.web_bind);
            if !origin_ok {
                tracing::warn!(event = "web_reject_origin", reason = "apply_origin");
                let body = serde_json::json!({ "error": "web_reject_origin" });
                return (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
            }
        } else {
            // Origin / Sec-Fetch-Site check: reject unless same-origin.
            let origin_ok = is_same_origin(&headers);
            if !origin_ok {
                tracing::warn!(event = "web_reject_origin", reason = "cross_site");
                let body = serde_json::json!({ "error": "web_reject_origin" });
                return (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
            }
        }
    }

    // ── WebSocket upgrade security: Origin check (CSWSH) ──────────────────
    if is_websocket_upgrade(&headers) {
        let origin_ok = is_same_origin(&headers);
        if !origin_ok {
            tracing::warn!(event = "web_reject_origin", reason = "cswsh_ws_upgrade");
            let body = serde_json::json!({ "error": "web_reject_origin" });
            return (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
        }
    }

    next.run(request).await
}

fn route_pattern_matches(pattern: &str, path: &str) -> bool {
    pattern
        .split('/')
        .zip(path.split('/'))
        .all(|(expected, actual)| expected.starts_with(':') || expected == actual)
        && pattern.split('/').count() == path.split('/').count()
}

/// Detect a WebSocket upgrade request by checking for the presence of both
/// `Upgrade: websocket` and `Connection: upgrade` headers.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let has_upgrade = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let has_connection = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("upgrade"));
    has_upgrade && has_connection
}

/// Returns `true` when the request's Origin / `Sec-Fetch-Site` headers
/// indicate same-origin or "none" (browser-initiated navigation).
fn is_same_origin(headers: &HeaderMap) -> bool {
    // Sec-Fetch-Site: "same-origin" or "none" → OK.
    if let Some(fetch_site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        let lower = fetch_site.to_lowercase();
        if lower == "same-origin" || lower == "none" {
            return true;
        }
        // "cross-site" → reject.
        return false;
    }

    // Fall back to Origin check (older browsers).
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        // If an Origin is present, it must be loopback.
        let origin_lower = origin.to_lowercase();
        return is_loopback_origin(&origin_lower);
    }

    // No Origin and no Sec-Fetch-Site → allow (non-browser client, or same-origin
    // browser that omitted these headers on same-origin POST).
    true
}

/// Returns `true` when the Origin URL's host is a loopback address.
fn is_loopback_origin(origin: &str) -> bool {
    // Strip the scheme
    let after_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    let host = extract_host_without_port(after_scheme);
    // Check if it looks like a bracketed IPv6 address.
    let host_clean = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&host);
    let ip: Result<IpAddr, _> = host_clean.parse();
    if let Ok(ip) = ip {
        return ip.is_loopback();
    }
    host_clean == "localhost"
}

/// Stricter Origin check for the config-apply write endpoint.
///
/// The Origin header MUST be present and MUST match
/// `http://<loopback>:<port>` where `<port>` equals the actual bound web
/// port.  Absent Origin or wrong-port loopback is rejected — this closes
/// the CSRF gap the generic `is_same_origin` leaves when the Origin is
/// absent.
fn check_apply_origin(headers: &HeaderMap, web_bind: std::net::SocketAddr) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return false; // absent Origin → reject
    };

    let origin_lower = origin.to_lowercase();

    // Must be http:// scheme.
    let Some(after_scheme) = origin_lower
        .strip_prefix("http://")
        .or_else(|| origin_lower.strip_prefix("https://"))
    else {
        return false;
    };

    // Check the host is loopback.
    if !is_loopback_origin(&origin_lower) {
        return false;
    }

    // Extract the port from the origin and require it matches the bound port.
    let origin_port = extract_port(after_scheme);
    origin_port == web_bind.port()
}

/// Extract the port from a `host[:port]` string (scheme already stripped).
/// Returns the parsed port if present, or `0` if no port was found.
fn extract_port(host: &str) -> u16 {
    // Bracket notation: after `[::1]:8080` → port is 8080.
    if host.starts_with('[') {
        if let Some(bracket_end) = host.find(']') {
            let rest = &host[bracket_end + 1..];
            if let Some(port_str) = rest.strip_prefix(':') {
                return port_str.parse().unwrap_or(0);
            }
        }
        return 0;
    }
    // Count colons: single colon = host:port, 2+ = bare IPv6.
    let colon_count = host.chars().filter(|&c| c == ':').count();
    if colon_count == 1
        && let Some(colon_pos) = host.find(':')
    {
        return host[colon_pos + 1..].parse().unwrap_or(0);
    }
    // No port (plain hostname or bare IPv6).
    0
}

/// Extract the host portion from `host[:port]`, correctly handling
/// bracketed IPv6 addresses like `[::1]:8080`, plain `localhost:8080`,
/// and bare IPv6 `::1`.
fn extract_host_without_port(host: &str) -> String {
    // Bracket notation: `[::1]:8080` or `[::1]` → take inside brackets.
    #[allow(clippy::collapsible_if)]
    if host.starts_with('[') {
        if let Some(bracket_end) = host.find(']') {
            return host[1..bracket_end].to_string();
        }
    }
    // Count colons to distinguish `host:port` (1 colon) from bare IPv6 (2+).
    let colon_count = host.chars().filter(|&c| c == ':').count();
    if colon_count == 1 {
        // Single colon → hostname:port, strip the port.
        if let Some(colon_pos) = host.find(':') {
            return host[..colon_pos].to_string();
        }
    }
    // 0 colons (plain hostname) or 2+ colons (bare IPv6) → keep as-is.
    host.to_string()
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::routing::get;
    use indexmap::IndexMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use tower::util::ServiceExt;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::ControlMsg;
    use dormant_doctor::DoctorService;

    fn test_web_state_with_bind(bind: SocketAddr) -> (WebState, CancellationToken) {
        let cancel = CancellationToken::new();

        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<ControlMsg>(8);
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

        let state = WebState::new(super::super::state::WebStateInner::new_for_test(
            super::super::state::WebStateInnerParams {
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
            },
        ));

        (state, cancel)
    }

    fn build_test_router(state: WebState) -> Router {
        Router::new()
            .route("/api/state", get(|| async { "ok" }))
            .route("/api/blank", axum::routing::post(|| async { "blanked" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                security_guard,
            ))
            .with_state(state)
    }

    // ── Host-header tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn rejects_foreign_host_header() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn accepts_loopback_host() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn accepts_localhost_host() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "localhost")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── IPv6 Host tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn accepts_ipv6_loopback_bracketed_host() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "[::1]:8080")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn accepts_ipv6_loopback_bare_host() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "[::1]")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn accepts_localhost_with_port() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "localhost:8080")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn accepts_ipv4_with_port() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "127.0.0.1:8080")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── POST Content-Type tests ────────────────────────────────────────────

    #[tokio::test]
    async fn accepts_loopback_json_post() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/blank")
            .header("Host", "127.0.0.1")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"display":"main"}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_cross_site_form_post() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        // POST with text/plain should be rejected (wrong Content-Type).
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/blank")
            .header("Host", "127.0.0.1")
            .header("Content-Type", "text/plain")
            .body(Body::from("not json"))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE
                || resp.status() == StatusCode::FORBIDDEN,
            "expected 415 or 403, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn rejects_cross_site_origin() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        // POST with a foreign Origin should be rejected.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/blank")
            .header("Host", "127.0.0.1")
            .header("Content-Type", "application/json")
            .header("Origin", "https://evil.com")
            .body(Body::from(r#"{"display":"main"}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allows_post_with_loopback_origin_with_port() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        // Origin: http://localhost:8080 should be allowed (same-origin loopback with port).
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/blank")
            .header("Host", "localhost:8080")
            .header("Content-Type", "application/json")
            .header("Origin", "http://localhost:8080")
            .body(Body::from(r#"{"display":"main"}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn allows_post_without_origin() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        // No Origin header (e.g. curl → ok).
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/blank")
            .header("Host", "127.0.0.1")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"display":"main"}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── WebSocket upgrade Origin tests ─────────────────────────────────────

    #[tokio::test]
    async fn rejects_cross_origin_websocket_upgrade() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        // GET with WS upgrade headers + cross-origin Origin → 403.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "127.0.0.1")
            .header("Upgrade", "websocket")
            .header("Connection", "upgrade")
            .header("Origin", "https://evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "cross-origin WS upgrade should be rejected"
        );
    }

    #[tokio::test]
    async fn allows_same_origin_websocket_upgrade() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let (state, _cancel) = test_web_state_with_bind(bind);
        let router = build_test_router(state);

        // GET with WS upgrade headers + no Origin (non-browser or same-origin) → passes guard.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/state")
            .header("Host", "127.0.0.1")
            .header("Upgrade", "websocket")
            .header("Connection", "upgrade")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        // Should NOT be 403 from the security guard (may be 200 or 426 from handler).
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    // ── Generalized strict-origin path set (Task 3) ────────────────────────

    /// M3 Must-2: a direct membership assertion, not merely coverage by the
    /// `⊆ STRICT ∪ WEAK` union check in `server.rs` — a mistaken WEAK-set
    /// classification for the not-yet-mounted pairing route must be caught
    /// HERE, at the classification site, the moment T5 mounts it.
    #[test]
    fn strict_origin_paths_contains_pair_samsung() {
        assert!(
            STRICT_ORIGIN_PATHS.contains(&"/api/pair/samsung"),
            "/api/pair/samsung must be pre-classified strict so the route \
             is strict-by-construction the moment it is mounted"
        );
    }

    /// M3 Must-2 counterpart for the global emergency-wake route: a direct
    /// membership assertion pinning it to the weak (same-origin) set, not
    /// merely coverage by the `⊆ STRICT ∪ WEAK` union check in `server.rs`.
    #[test]
    fn emergency_wake_is_explicitly_acknowledged_weak() {
        assert!(ACKNOWLEDGED_WEAK_ROUTES.contains(&"/api/emergency-wake"));
        assert!(!STRICT_ORIGIN_PATHS.contains(&"/api/emergency-wake"));
    }

    #[test]
    fn exercise_is_explicitly_acknowledged_weak() {
        assert!(ACKNOWLEDGED_WEAK_ROUTES.contains(&"/api/doctor/exercise/:display"));
        assert!(!STRICT_ORIGIN_PATHS.contains(&"/api/doctor/exercise/:display"));
    }

    /// Structural comment-test (no path-normalization layer): the guard
    /// compares `request.uri().path()` LITERALLY against
    /// `STRICT_ORIGIN_PATHS` / `ACKNOWLEDGED_WEAK_ROUTES` — there is no
    /// `tower_http::normalize_path` (or similar) layer mounted above
    /// `security_guard` in `server.rs::build_router`. That raw comparison
    /// is only safe because every entry here is already in axum's
    /// canonical route-path form. If a normalization layer is ever added
    /// upstream of the guard, this raw comparison would need to normalize
    /// its input the same way, or a differently-spelled-but-equivalent
    /// path (e.g. a trailing slash) could route to the handler while
    /// silently missing the strict check.
    #[test]
    fn strict_and_weak_route_paths_are_already_normalized() {
        for path in STRICT_ORIGIN_PATHS
            .iter()
            .chain(ACKNOWLEDGED_WEAK_ROUTES.iter())
        {
            assert!(path.starts_with("/api/"), "{path} must be a full /api path");
            assert!(
                !path.ends_with('/'),
                "{path} must not have a trailing slash"
            );
            assert!(
                !path.contains("//"),
                "{path} must not contain a double slash"
            );
            assert_eq!(
                *path,
                path.to_ascii_lowercase(),
                "{path} must already be lowercase"
            );
        }
    }
}
