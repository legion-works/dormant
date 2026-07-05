//! Security guard — Host-header allow-list, CSRF defence, CORS hardening.
//!
//! Every request passes through the Host check unconditionally (anti
//! DNS-rebind).  State-changing methods (`POST`) additionally require
//! `Content-Type: application/json` and a same-origin disposition.
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

/// Allowed loopback host values (Hard-coded — the configured bind is always
/// loopback unless `web_allow_nonloopback` is explicitly set, in which case
/// the daemon has already warned at startup).
const ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];

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
            let host_only = host_lower.split(':').next().unwrap_or(&host_lower);
            ALLOWED_HOSTS.contains(&host_only) || host_only == state.inner.web_bind.ip().to_string()
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

        // Origin / Sec-Fetch-Site check: reject unless same-origin.
        let origin_ok = is_same_origin(&headers);
        if !origin_ok {
            tracing::warn!(event = "web_reject_origin", reason = "cross_site");
            let body = serde_json::json!({ "error": "web_reject_origin" });
            return (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
        }
    }

    next.run(request).await
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
    let host = after_scheme.split(':').next().unwrap_or(after_scheme);
    let ip: Result<IpAddr, _> = host.parse();
    if let Ok(ip) = ip {
        return ip.is_loopback();
    }
    host == "localhost"
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
    use tokio_util::sync::CancellationToken;
    use tower::util::ServiceExt;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};
    use dormant_core::rules::ControlMsg;
    use dormant_doctor::DoctorService;

    fn test_web_state_with_bind(bind: SocketAddr) -> (WebState, CancellationToken) {
        let cancel = CancellationToken::new();

        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) = tokio::sync::mpsc::channel::<()>(8);
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

        let (config_tx, config_rx) = tokio::sync::watch::channel(config);
        let (creds_tx, creds_rx) = tokio::sync::watch::channel(creds);

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor = DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let state = WebState::new(super::super::state::WebStateInner {
            ctl_tx: ctl_tx.clone(),
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            doctor,
            web_bind: bind,
            cancel: cancel.clone(),
        });

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
}
