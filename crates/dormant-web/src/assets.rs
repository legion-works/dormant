//! Static asset serving — embeds the frontend `dist/` at compile time
//! and serves files with an SPA fallback for client-side routing.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// Compiled-in frontend bundle.  The folder must exist at build time;
/// before Task 8 lands a placeholder `index.html` keeps it non-empty.
#[derive(RustEmbed)]
#[folder = "webui/dist"]
struct Assets;

/// Fallback handler for non-`/api` paths.
///
/// Tries to serve the request path as a static asset from the embedded
/// `webui/dist` folder.  If the asset does not exist, serves `index.html`
/// instead so the client-side router can handle the URL.  `/api/*`
/// requests are NOT routed here — they stay with the axum router and
/// return 404 on miss.
pub(crate) async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    let asset_path = if path.is_empty() { "index.html" } else { path };

    // Exact match first — serve the real asset with the correct MIME.
    if let Some(file) = Assets::get(asset_path) {
        let mime = mime_guess::from_path(asset_path).first_or_text_plain();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(file.data))
            .unwrap();
    }

    // SPA fallback — serve index.html so the client-side router can render.
    if let Some(file) = Assets::get("index.html") {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(file.data))
            .unwrap();
    }

    // Neither the asset nor index.html exists (broken build).
    (
        StatusCode::NOT_FOUND,
        "404: index.html not found in embedded assets",
    )
        .into_response()
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use axum::routing::get;
    use tower::util::ServiceExt;

    /// Build a minimal router that nests `/api` routes and uses the SPA
    /// fallback for everything else, matching the real `build_router`
    /// structure.
    fn test_router() -> Router {
        let api = Router::new()
            .route("/ping", get(|| async { "pong" }))
            .fallback(|| async { (StatusCode::NOT_FOUND, "") });

        Router::new().nest("/api", api).fallback(spa_fallback)
    }

    // ── SPA routing tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn root_serves_index_html() {
        let router = test_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        assert_eq!(ct, Some("text/html"), "GET / should serve text/html");
    }

    #[tokio::test]
    async fn spa_route_falls_back_to_index() {
        let router = test_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/some/spa/route")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        assert_eq!(
            ct,
            Some("text/html"),
            "SPA route fallback should serve text/html"
        );
    }

    #[tokio::test]
    async fn api_miss_returns_404_not_spa_fallback() {
        let router = test_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        assert_ne!(
            ct,
            Some("text/html"),
            "API miss must NOT return the SPA fallback (not text/html)"
        );

        // Assert the body is the API-404 body, not the SPA index.
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("should be able to read response body");
        let body_str =
            std::str::from_utf8(&body_bytes).expect("API 404 body should be valid UTF-8");
        assert!(
            !body_str.contains("PLACEHOLDER"),
            "API 404 body must not contain the placeholder/index HTML"
        );
        assert!(
            !body_str.contains("<html"),
            "API 404 body must not contain <html (SPA index leaked into API miss)"
        );
    }

    #[tokio::test]
    async fn api_route_still_works() {
        let router = test_router();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/ping")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }
}
