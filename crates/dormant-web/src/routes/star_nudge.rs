//! `POST /api/star-nudge/dismiss` + `POST /api/star-nudge/star` —
//! persist the sidebar "Star the repo" nudge dismissal so it never
//! renders again. The star route additionally attempts to star the repo
//! via `gh api` before dismissing.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::WebState;
use crate::error::WebError;
use crate::routes::dismiss_flag::write_dismiss_flag;

/// Fixed PATH for the `gh` child process — never inherits the daemon's
/// session PATH (which may contain user-writable directories like
/// `~/.local/bin` that could shadow `gh`). Matches the hooks module's
/// `HOOK_CHILD_PATH`.
const GH_CHILD_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Response shape for `POST /api/star-nudge/star`.
#[derive(Debug, Serialize)]
pub(crate) struct StarResponse {
    starred: bool,
}

/// Thin wrapper around a `gh` binary path.  Production uses `GhStar::new()`
/// (lookup on [`GH_CHILD_PATH`]); tests inject an explicit path via
/// `GhStar::at`.
struct GhStar {
    program: PathBuf,
    /// Test-only PATH override — when `Some`, the child process uses this
    /// PATH instead of the fixed production constant.  None in production.
    #[cfg_attr(not(test), allow(dead_code))]
    test_path: Option<PathBuf>,
}

impl GhStar {
    fn new() -> Self {
        Self {
            program: PathBuf::from("gh"),
            test_path: None,
        }
    }

    #[cfg(test)]
    fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            test_path: None,
        }
    }

    /// Run `gh api -X PUT user/starred/legion-works/dormant` with a fixed
    /// child environment (clear env, set PATH + HOME only) and 5 s timeout.
    /// Returns `true` iff gh exists AND exits 0.
    async fn star(&self, timeout: Duration) -> bool {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(["api", "-X", "PUT", "user/starred/legion-works/dormant"]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        // SEC S1: never inherit the daemon's env — clear it and set only the
        // fixed PATH (or the test-injected PATH) plus HOME so gh can find
        // ~/.config/gh/hosts.yml for auth.
        cmd.env_clear();
        let path = self.test_path.as_ref().map_or_else(
            || std::ffi::OsString::from(GH_CHILD_PATH),
            |p| p.as_os_str().to_os_string(),
        );
        cmd.env("PATH", path);
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("HOME", home);
        }

        let child = cmd.output();

        match tokio::time::timeout(timeout, child).await {
            Err(_) => {
                tracing::debug!(event = "star_nudge_gh_timeout");
                false
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(event = "star_nudge_gh_not_found");
                false
            }
            Ok(Err(e)) => {
                tracing::warn!(event = "star_nudge_gh_spawn_error", error = %e);
                false
            }
            Ok(Ok(output)) => {
                let ok = output.status.success();
                if !ok {
                    tracing::debug!(event = "star_nudge_gh_nonzero", code = output.status.code(),);
                }
                ok
            }
        }
    }
}

/// `POST /api/star-nudge/dismiss` — write the `star-nudge-dismissed` flag
/// file so the nudge never renders again. Idempotent.
pub(crate) async fn post_star_nudge_dismiss(
    State(state): State<WebState>,
) -> Result<impl IntoResponse, WebError> {
    let path = &state.inner.star_nudge_path;
    if path.exists() {
        tracing::debug!(event = "star_nudge_dismissed", ?path, "already dismissed");
        return Ok(StatusCode::NO_CONTENT);
    }
    write_dismiss_flag(path, "star_nudge_dismissed")?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/star-nudge/star` — attempt to star the repo via `gh api`,
/// then write the dismiss flag regardless of outcome.
///
/// Returns `{ "starred": true }` when gh exists AND exits 0;
/// `{ "starred": false }` otherwise (gh missing, not authed, timeout,
/// nonzero exit). The nudge is dismissed either way.
pub(crate) async fn post_star_nudge_star(
    State(state): State<WebState>,
) -> Result<Json<StarResponse>, WebError> {
    let mut gh = GhStar::new();
    // CORR 1: test-injected explicit gh path (None in production).
    if let Some(ref p) = state.inner.star_gh_path {
        gh.program = p.clone();
    }
    if let Some(ref d) = state.inner.star_test_path {
        gh.test_path = Some(d.clone());
    }
    let starred = gh.star(Duration::from_secs(5)).await;
    write_dismiss_flag(&state.inner.star_nudge_path, "star_nudge_dismissed")?;

    if starred {
        tracing::info!(event = "star_nudge_starred");
    }
    Ok(Json(StarResponse { starred }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use indexmap::IndexMap;
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use tower::util::ServiceExt;

    use dormant_core::config::schema::{Config, Credentials, DaemonConfig};

    use crate::server;
    use crate::state::{WebStateInner, WebStateInnerParams};

    /// Build a [`WebState`] whose `config_path` lives under `dir`, so
    /// the derived `star_nudge_path` is `dir/star-nudge-dismissed`.
    fn state_in_dir(dir: &std::path::Path) -> (WebState, CancellationToken) {
        let cancel = CancellationToken::new();
        let (ctl_tx, _ctl_rx) = mpsc::channel::<dormant_core::rules::ControlMsg>(8);
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
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
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
            reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
            reload_rx,
            config_rx,
            creds_rx,
            config_path: dir.join("config.toml"),
            creds_path: dir.join("config.creds.toml"),
            doctor,
            wear: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            cancel: cancel.clone(),
            reload_timeout: std::time::Duration::from_secs(10),
            wear_sampling_rx: tokio::sync::watch::channel(None).1,
        }));

        (state, cancel)
    }

    /// Create a stub executable at `dir/gh` that exits 0 and prints nothing.
    /// Returns the path to the stub.
    fn write_stub_gh(dir: &std::path::Path) -> std::path::PathBuf {
        let stub = dir.join("gh");
        let mut f = std::fs::File::create(&stub).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "exit 0").unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        stub
    }

    // ── Dismiss tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn dismiss_writes_flag_file_and_returns_204() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _cancel) = state_in_dir(dir.path());
        let flag_path = dir.path().join("star-nudge-dismissed");
        assert!(!flag_path.exists());

        let result = post_star_nudge_dismiss(State(state)).await;
        assert!(result.is_ok());
        let response = result.unwrap().into_response();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert!(flag_path.exists());
        assert_eq!(std::fs::read_to_string(&flag_path).unwrap(), "dismissed\n");
    }

    #[tokio::test]
    async fn dismiss_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _cancel) = state_in_dir(dir.path());
        let flag_path = dir.path().join("star-nudge-dismissed");

        // First dismiss
        let result = post_star_nudge_dismiss(State(state.clone())).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().into_response().status(),
            StatusCode::NO_CONTENT
        );
        let mtime = flag_path.metadata().unwrap().modified().unwrap();

        // Second dismiss — must succeed and not rewrite the file
        let result = post_star_nudge_dismiss(State(state.clone())).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().into_response().status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(flag_path.metadata().unwrap().modified().unwrap(), mtime);
    }

    /// Pin the literal log event name emitted by `post_star_nudge_dismiss`.
    /// The pre-extraction (`5a18e4a`) literal was `star_nudge_dismissed`;
    /// the extraction must preserve it byte-for-byte. A regression that
    /// derives the name from the filename (`star_nudge_dismissed_dismissed`)
    /// would silently break dashboard greps and operator alerting.
    #[tokio::test]
    async fn dismiss_emits_star_nudge_dismissed_event_literal() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _cancel) = state_in_dir(dir.path());

        crate::test_support::start_capturing();
        let result = post_star_nudge_dismiss(State(state)).await;
        assert!(result.is_ok());
        let events = crate::test_support::take_captured();

        // Event is captured as `event="star_nudge_dismissed"` (the
        // `FieldDumpVisitor` formats Debug fields with `name={value:?}`).
        assert!(
            events
                .iter()
                .any(|e| e.contains("event=\"star_nudge_dismissed\"")),
            "missing literal event=star_nudge_dismissed in captured events: {events:?}"
        );
        // Negative pin: the doubled-suffix regression would emit
        // `event="star_nudge_dismissed_dismissed"` — guard against it.
        assert!(
            !events
                .iter()
                .any(|e| e.contains("star_nudge_dismissed_dismissed")),
            "do NOT derive the event name from the filename — silently emits the doubled suffix \
             and breaks grep-based monitoring; pass the literal at the call site instead"
        );
    }

    #[tokio::test]
    async fn get_daemon_reflects_dismiss_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _cancel) = state_in_dir(dir.path());
        let _bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let router = server::build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::get("/api/daemon")
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["star_nudge_dismissed"], false);

        // Dismiss
        let _ = std::fs::write(dir.path().join("star-nudge-dismissed"), "dismissed\n");

        let (state2, _cancel2) = state_in_dir(dir.path());
        let router2 = server::build_router(state2);
        let response = router2
            .oneshot(
                Request::get("/api/daemon")
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["star_nudge_dismissed"], true);
    }

    // ── Star unit tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn star_gh_stub_exits_zero_returns_starred_true() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_stub_gh(dir.path());

        let gh = GhStar::at(&stub);
        let starred = gh.star(Duration::from_secs(2)).await;
        assert!(starred, "stub gh exits 0 → starred should be true");
    }

    #[tokio::test]
    async fn star_gh_missing_returns_starred_false() {
        // Point at a path that definitely doesn't exist.
        let gh = GhStar::at("/no/such/gh/binary/ever");
        let starred = gh.star(Duration::from_secs(2)).await;
        assert!(!starred);
    }

    // ── Route-level star tests ───────────────────────────────────────────

    #[tokio::test]
    async fn post_star_route_returns_starred_false_when_gh_missing() {
        // CORR 1: exercises the ACTUAL route handler, not GhStar directly.
        let dir = tempfile::tempdir().unwrap();
        let flag_path = dir.path().join("star-nudge-dismissed");
        assert!(!flag_path.exists());

        // Construct a fresh state with a nonexistent gh path injected.
        let (mut state, _cancel) = state_in_dir(dir.path());
        let inner_ref =
            Arc::get_mut(&mut state.inner).expect("state must have a unique strong reference");
        inner_ref.star_gh_path = Some(PathBuf::from("/no/such/gh/binary/ever"));

        let result = post_star_nudge_star(State(state)).await;
        assert!(result.is_ok());
        assert!(
            !result.unwrap().starred,
            "starred must be false when gh is missing"
        );

        // Flag file must be written regardless of gh outcome.
        assert!(flag_path.exists());
    }

    #[tokio::test]
    async fn post_star_route_returns_starred_true_with_stub_injected() {
        // Uses the test-only PATH override on GhStar (via star_test_path
        // seam) so the stub directory is injected without mutating global
        // PATH — no parallel-test race.
        let dir = tempfile::tempdir().unwrap();
        write_stub_gh(dir.path());
        let (mut state, _cancel) = state_in_dir(dir.path());
        let flag_path = dir.path().join("star-nudge-dismissed");
        assert!(!flag_path.exists());

        let inner_ref =
            Arc::get_mut(&mut state.inner).expect("state must have a unique strong reference");
        inner_ref.star_test_path = Some(dir.path().to_path_buf());

        let result = post_star_nudge_star(State(state)).await;
        assert!(result.is_ok());
        assert!(
            result.unwrap().starred,
            "starred must be true when stub gh is on the injected test PATH"
        );

        assert!(flag_path.exists());
    }

    // ── Strict-origin guard for /api/star-nudge/star ───────────────────

    #[tokio::test]
    async fn star_route_rejects_absent_origin() {
        let dir = tempfile::tempdir().unwrap();
        let (mut state, _cancel) = state_in_dir(dir.path());
        // Inject a nonexistent gh path so the handler doesn't reach
        // out to a real gh binary during the guard test.
        let inner_ref =
            Arc::get_mut(&mut state.inner).expect("state must have a unique strong reference");
        inner_ref.star_gh_path = Some(PathBuf::from("/no/such/gh/binary/ever"));
        let router = server::build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/star-nudge/star")
                    .header("Host", "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // STRICT_ORIGIN_PATHS requires the Origin header — absent Origin
        // must be rejected with 403.
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn star_route_allows_loopback_origin() {
        let dir = tempfile::tempdir().unwrap();
        let (mut state, _cancel) = state_in_dir(dir.path());
        let inner_ref =
            Arc::get_mut(&mut state.inner).expect("state must have a unique strong reference");
        inner_ref.star_gh_path = Some(PathBuf::from("/no/such/gh/binary/ever"));
        let router = server::build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/star-nudge/star")
                    .header("Host", "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Origin", "http://127.0.0.1:8080")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Valid loopback Origin passes the strict guard — the handler
        // runs and returns 200 (starred:false when gh is missing).
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Edge case: relative config path ──────────────────────────────────

    #[test]
    fn star_nudge_path_falls_back_to_cwd_when_config_parent_is_empty() {
        // With a bare relative config filename, parent() returns Some("")
        // which is filtered out; the fallback "." kicks in.
        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<dormant_core::rules::ControlMsg>(8);
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
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);
        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);
        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        let inner = WebStateInner::new_for_test(WebStateInnerParams {
            ctl_tx,
            reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
            reload_rx,
            config_rx,
            creds_rx,
            // Bare relative filename — parent() filters to empty, falls back to "."
            config_path: std::path::PathBuf::from("config.toml"),
            creds_path: std::path::PathBuf::from("config.creds.toml"),
            doctor,
            wear: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            cancel: tokio_util::sync::CancellationToken::new(),
            reload_timeout: std::time::Duration::from_secs(10),
            wear_sampling_rx: tokio::sync::watch::channel(None).1,
        });

        assert_eq!(
            inner.star_nudge_path,
            std::path::Path::new("./star-nudge-dismissed")
        );
    }
}
