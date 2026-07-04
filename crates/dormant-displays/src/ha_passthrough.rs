//! Home Assistant service-call passthrough display controller.
//!
//! Makes every HA-controllable device a dormant display by calling HA's
//! `api/services/{domain}/{service}` REST endpoint with a bearer token.
//! The user declares the service strings (e.g. `"remote.send_command"`) and
//! optional JSON bodies in the display config; the controller splits the
//! service string at the first `.` into domain and service parts.
//!
//! ## Why a separate controller instead of a generic HTTP command?
//!
//! HA passthrough is a first-class controller because it carries HA-specific
//! semantics: bearer auth, the `/api/services/` URL convention, a dedicated
//! availability probe (`GET /api/`), and a 5-second default timeout.  A
//! generic HTTP command controller would need to re-implement all of these
//! in the config surface.

use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::DormantError;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

/// Default HTTP request timeout for HA API calls.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Home Assistant service-call passthrough controller.
///
/// Constructed by [`crate::registry::build_controllers`] from a
/// [`dormant_core::config::schema::DisplayConfig`] that names `ha-passthrough`
/// as one of its controllers.
pub struct HaPassthroughController {
    /// Base URL of the Home Assistant instance (e.g. `http://ha.local:8123`).
    base_url: String,
    /// Long-lived access token for bearer auth.
    token: String,
    /// Domain part of the blank service (before the first `.`).
    blank_domain: String,
    /// Service part of the blank service (after the first `.`).
    blank_service: String,
    /// JSON body for the blank service call (empty object if `None`).
    blank_data: serde_json::Value,
    /// Domain part of the wake service.
    wake_domain: String,
    /// Service part of the wake service.
    wake_service: String,
    /// JSON body for the wake service call (empty object if `None`).
    wake_data: serde_json::Value,
    /// Declared blank modes from the display config.
    modes: Vec<BlankMode>,
    /// Shared HTTP client with a configurable timeout.
    client: reqwest::Client,
}

impl std::fmt::Debug for HaPassthroughController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HaPassthroughController")
            .field("base_url", &self.base_url)
            .field("token", &"***")
            .field("blank_domain", &self.blank_domain)
            .field("blank_service", &self.blank_service)
            .field("blank_data", &self.blank_data)
            .field("wake_domain", &self.wake_domain)
            .field("wake_service", &self.wake_service)
            .field("wake_data", &self.wake_data)
            .field("modes", &self.modes)
            .field("client", &"reqwest::Client")
            .finish()
    }
}

impl HaPassthroughController {
    /// Build a new `HaPassthroughController`.
    ///
    /// # Errors
    ///
    /// - [`DormantError::ConfigInvalid`] if `blank_service` or `wake_service`
    ///   does not contain a `.` (i.e. no domain/service split possible).
    pub fn new(
        ha_url: String,
        token: String,
        blank_service: &str,
        blank_data: Option<toml::Value>,
        wake_service: &str,
        wake_data: Option<toml::Value>,
        modes: Vec<BlankMode>,
    ) -> Result<Self, DormantError> {
        let (blank_domain, blank_svc) =
            split_service(blank_service).ok_or_else(|| DormantError::ConfigInvalid {
                detail: format!(
                    "blank_service '{blank_service}' must be in 'domain.service' format"
                ),
            })?;
        let (wake_domain, wake_svc) =
            split_service(wake_service).ok_or_else(|| DormantError::ConfigInvalid {
                detail: format!("wake_service '{wake_service}' must be in 'domain.service' format"),
            })?;

        Ok(Self {
            base_url: ha_url,
            token,
            blank_domain,
            blank_service: blank_svc,
            blank_data: toml_to_json(blank_data),
            wake_domain,
            wake_service: wake_svc,
            wake_data: toml_to_json(wake_data),
            modes,
            client: Self::build_client(DEFAULT_TIMEOUT),
        })
    }

    /// Build a controller with a custom timeout (used in tests).
    #[allow(dead_code, clippy::too_many_arguments)]
    fn with_timeout(
        ha_url: String,
        token: String,
        blank_service: &str,
        blank_data: Option<toml::Value>,
        wake_service: &str,
        wake_data: Option<toml::Value>,
        modes: Vec<BlankMode>,
        timeout: Duration,
    ) -> Result<Self, DormantError> {
        let (blank_domain, blank_svc) =
            split_service(blank_service).ok_or_else(|| DormantError::ConfigInvalid {
                detail: format!(
                    "blank_service '{blank_service}' must be in 'domain.service' format"
                ),
            })?;
        let (wake_domain, wake_svc) =
            split_service(wake_service).ok_or_else(|| DormantError::ConfigInvalid {
                detail: format!("wake_service '{wake_service}' must be in 'domain.service' format"),
            })?;

        Ok(Self {
            base_url: ha_url,
            token,
            blank_domain,
            blank_service: blank_svc,
            blank_data: toml_to_json(blank_data),
            wake_domain,
            wake_service: wake_svc,
            wake_data: toml_to_json(wake_data),
            modes,
            client: Self::build_client(timeout),
        })
    }

    /// Build a `reqwest::Client` with the given timeout.
    fn build_client(timeout: Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest::Client::builder should never fail with default settings")
    }

    /// POST to `{base_url}/api/services/{domain}/{service}` with bearer auth
    /// and the given JSON body.
    async fn call_service(
        &self,
        domain: &str,
        service: &str,
        body: &serde_json::Value,
    ) -> Result<(), CmdFailure> {
        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/api/services/{domain}/{service}");

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    Ok(())
                } else {
                    let body_tail = resp.text().await.unwrap_or_default();
                    let tail = truncate_body(&body_tail, 200);
                    Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: HA service call returned {status}; {tail}"),
                    })
                }
            }
            Err(e) => {
                if e.is_timeout() {
                    Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: HA request timed out"),
                    })
                } else {
                    Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: HA request failed: {e}"),
                    })
                }
            }
        }
    }
}

impl HaPassthroughController {
    /// Literal controller name — grep-stable, matches the `ha-passthrough` config type.
    const NAME: &'static str = "ha-passthrough";
}

#[async_trait]
impl DisplayController for HaPassthroughController {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        self.modes.clone()
    }

    async fn is_available(&self) -> bool {
        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/api/");
        match self
            .client
            .get(&url)
            .timeout(Duration::from_secs(2))
            .bearer_auth(&self.token)
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        if !self.modes.contains(&mode) {
            return Err(CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!(
                    "{E_DISPLAY_IO}: mode {mode:?} not in declared modes {:?}",
                    self.modes
                ),
            });
        }
        self.call_service(&self.blank_domain, &self.blank_service, &self.blank_data)
            .await
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        self.call_service(&self.wake_domain, &self.wake_service, &self.wake_data)
            .await
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────────

/// Split a `"domain.service"` string at the first `.`.
///
/// Returns `None` if there is no `.` or either part is empty.
fn split_service(s: &str) -> Option<(String, String)> {
    let dot = s.find('.')?;
    let domain = s[..dot].to_string();
    let service = s[dot + 1..].to_string();
    if domain.is_empty() || service.is_empty() {
        return None;
    }
    Some((domain, service))
}

/// Convert an optional `toml::Value` to a `serde_json::Value`.
///
/// - `None` → `serde_json::Value::Object(Default::default())` (empty object)
/// - Tables → JSON objects
/// - Arrays → JSON arrays
/// - Strings → JSON strings
/// - Integers → JSON numbers
/// - Floats → JSON numbers
/// - Booleans → JSON booleans
/// - Datetimes → JSON strings (ISO 8601 representation)
fn toml_to_json(v: Option<toml::Value>) -> serde_json::Value {
    match v {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(toml_val) => convert_toml_value(toml_val),
    }
}

fn convert_toml_value(v: toml::Value) -> serde_json::Value {
    match v {
        toml::Value::Table(t) => {
            let map: serde_json::Map<String, serde_json::Value> = t
                .into_iter()
                .map(|(k, v)| (k, convert_toml_value(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(convert_toml_value).collect())
        }
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => {
            if let Some(n) = serde_json::Number::from_f64(f) {
                serde_json::Value::Number(n)
            } else {
                serde_json::Value::String(f.to_string())
            }
        }
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
    }
}

/// Truncate a string to at most `max` characters, keeping the tail.
fn truncate_body(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let start = s.len() - max;
        // Find the nearest char boundary so we don't split a multi-byte char.
        let mut idx = start;
        while idx < s.len() && !s.is_char_boundary(idx) {
            idx += 1;
        }
        s[idx..].to_string()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args, clippy::approx_constant)]
mod tests {
    use super::*;
    use dormant_core::error::E_DISPLAY_IO;
    use wiremock::matchers::{bearer_token, body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Service string parsing ──────────────────────────────────────────────

    #[test]
    fn service_string_without_dot_rejected_at_build() {
        let err = HaPassthroughController::new(
            "http://ha.local:8123".into(),
            "tok".into(),
            "noservice",
            None,
            "remote.send_command",
            None,
            vec![BlankMode::PowerOff],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("blank_service"),
            "error should mention blank_service: {}",
            err
        );
        assert!(
            err.to_string().contains("domain.service"),
            "error should mention domain.service format: {}",
            err
        );
    }

    #[test]
    fn wake_service_without_dot_rejected_at_build() {
        let err = HaPassthroughController::new(
            "http://ha.local:8123".into(),
            "tok".into(),
            "remote.send_command",
            None,
            "nowake",
            None,
            vec![BlankMode::PowerOff],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("wake_service"),
            "error should mention wake_service: {}",
            err
        );
    }

    // ── toml_to_json conversion ─────────────────────────────────────────────

    #[test]
    fn toml_to_json_conversion() {
        // Build a nested TOML value with a flat array, nested table, and scalars.
        let toml_str = r#"
table = { key = "val" }
list = [1, true, 3.14]
flag = false
"#;
        let toml_val: toml::Value = toml::from_str(toml_str).unwrap();
        let json = toml_to_json(Some(toml_val));

        // Verify structure via serde_json
        assert_eq!(json["table"]["key"], "val");
        assert_eq!(json["list"][0], 1);
        assert_eq!(json["list"][1], true);
        let val = json["list"][2].as_f64().unwrap();
        assert!((val - 3.14_f64).abs() < 1e-10);
        assert_eq!(json["flag"], false);
    }

    #[test]
    fn toml_none_becomes_empty_object() {
        let json = toml_to_json(None);
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn toml_datetime_becomes_string() {
        let toml_val: toml::Value = toml::from_str("dt = 1979-05-27T07:32:00Z").unwrap();
        let json = toml_to_json(Some(toml_val));
        assert_eq!(json["dt"], "1979-05-27T07:32:00Z");
    }

    // ── Wiremock-driven HTTP tests ──────────────────────────────────────────

    /// Helper: build a controller pointed at a wiremock server.
    fn test_controller(mock_server: &MockServer) -> HaPassthroughController {
        HaPassthroughController::new(
            mock_server.uri(),
            "test-token".into(),
            "remote.send_command",
            Some(toml::from_str("key = \"value\"").unwrap()),
            "remote.wake",
            None,
            vec![BlankMode::PowerOff],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn blank_posts_correct_url_body_and_bearer() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/services/remote/send_command"))
            .and(bearer_token("test-token"))
            .and(header("content-type", "application/json"))
            .and(body_json(serde_json::json!({"key": "value"})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let ctrl = test_controller(&mock);
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
    }

    #[tokio::test]
    async fn blank_with_trailing_slash_base_url() {
        // Trailing slash on base_url must be trimmed so the path doesn't
        // become `//api/services/...`.
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/services/remote/send_command"))
            .and(bearer_token("test-token"))
            .and(body_json(serde_json::json!({"key": "value"})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let ctrl = HaPassthroughController::new(
            format!("{}/", mock.uri()), // trailing slash
            "test-token".into(),
            "remote.send_command",
            Some(toml::from_str("key = \"value\"").unwrap()),
            "remote.wake",
            None,
            vec![BlankMode::PowerOff],
        )
        .unwrap();
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
    }

    #[tokio::test]
    async fn wake_posts_wake_service() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/services/remote/wake"))
            .and(bearer_token("test-token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let ctrl = test_controller(&mock);
        ctrl.wake().await.unwrap();
    }

    #[tokio::test]
    async fn non_2xx_maps_cmdfailure_with_status() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/services/remote/send_command"))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .mount(&mock)
            .await;

        let ctrl = test_controller(&mock);
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert_eq!(err.controller, "ha-passthrough");
        assert!(
            err.error.starts_with(E_DISPLAY_IO),
            "error must start with {E_DISPLAY_IO}: {}",
            err.error
        );
        assert!(
            err.error.contains("503"),
            "error should contain status 503: {}",
            err.error
        );
        assert!(
            err.error.contains("Service Unavailable"),
            "error should contain body tail: {}",
            err.error
        );
    }

    #[tokio::test]
    async fn is_available_2xx_true() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/"))
            .and(bearer_token("test-token"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;

        let ctrl = test_controller(&mock);
        assert!(ctrl.is_available().await);
    }

    #[tokio::test]
    async fn is_available_500_false() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let ctrl = test_controller(&mock);
        assert!(!ctrl.is_available().await);
    }

    #[tokio::test]
    async fn timeout_maps_err() {
        let mock = MockServer::start().await;

        // Delay longer than the 5s default — use with_timeout with a short
        // timeout so the test doesn't actually take 5s.
        Mock::given(method("POST"))
            .and(path("/api/services/remote/send_command"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&mock)
            .await;

        let ctrl = HaPassthroughController::with_timeout(
            mock.uri(),
            "test-token".into(),
            "remote.send_command",
            None,
            "remote.wake",
            None,
            vec![BlankMode::PowerOff],
            Duration::from_millis(100),
        )
        .unwrap();

        let start = std::time::Instant::now();
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "blank should be bounded by ~100ms timeout, took {elapsed:?}",
        );
        assert!(
            err.error.contains("timed out"),
            "error should mention timeout: {}",
            err.error
        );
    }

    #[tokio::test]
    async fn mode_not_declared_rejected() {
        let mock = MockServer::start().await;
        let ctrl = HaPassthroughController::new(
            mock.uri(),
            "tok".into(),
            "remote.send_command",
            None,
            "remote.wake",
            None,
            vec![BlankMode::ScreenOffAudioOn],
        )
        .unwrap();

        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert_eq!(err.controller, "ha-passthrough");
        assert!(err.error.contains("not in declared modes"));
    }

    // ── split_service ───────────────────────────────────────────────────────

    #[test]
    fn split_service_normal() {
        let (domain, service) = split_service("remote.send_command").unwrap();
        assert_eq!(domain, "remote");
        assert_eq!(service, "send_command");
    }

    #[test]
    fn split_service_no_dot_is_none() {
        assert!(split_service("noservice").is_none());
    }

    #[test]
    fn split_service_empty_domain_is_none() {
        assert!(split_service(".service").is_none());
    }

    #[test]
    fn split_service_empty_service_is_none() {
        assert!(split_service("domain.").is_none());
    }

    #[test]
    fn split_service_multiple_dots_uses_first() {
        let (domain, service) = split_service("a.b.c").unwrap();
        assert_eq!(domain, "a");
        assert_eq!(service, "b.c");
    }
}
