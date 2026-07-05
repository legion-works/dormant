//! `GET /api/config` — config inventory + raw TOML + validation.
//!
//! Returns the last-applied `Config` from the live watch together with the
//! on-disk raw TOML re-read at request time and its validation result.
//! Inline secrets (URL userinfo) are redacted in both `inventory` and
//! `raw_toml` before serving.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use dormant_core::config::schema::{Config, MqttSensorCfg, SensorConfig};
use dormant_core::config::{
    Strictness, ValidationError, Warning, load_config, load_credentials, validate,
};
use dormant_displays::registry::capabilities;
use serde::Serialize;

use crate::WebState;
use crate::error::WebError;

/// Serializable wrapper for [`Warning`].
#[derive(Serialize, Debug)]
pub(crate) struct SerializableWarning {
    key_path: String,
    message: String,
}

impl From<&Warning> for SerializableWarning {
    fn from(w: &Warning) -> Self {
        Self {
            key_path: w.key_path.clone(),
            message: w.message.clone(),
        }
    }
}

/// Serializable wrapper for [`ValidationError`].
#[derive(Serialize, Debug)]
pub(crate) struct SerializableValidationError {
    what: String,
    detail: String,
}

impl From<&ValidationError> for SerializableValidationError {
    fn from(e: &ValidationError) -> Self {
        Self {
            what: e.what.clone(),
            detail: e.detail.clone(),
        }
    }
}

/// Response shape for `GET /api/config` (spec §4.1).
#[derive(Serialize, Debug)]
pub(crate) struct ConfigResponse {
    pub(crate) path: String,
    pub(crate) config_version: u32,
    pub(crate) source: &'static str,
    pub(crate) raw_toml: String,
    pub(crate) inventory: Config,
    pub(crate) validation: ConfigValidation,
}

#[derive(Serialize, Debug)]
pub(crate) struct ConfigValidation {
    pub(crate) ok: bool,
    pub(crate) warnings: Vec<SerializableWarning>,
    pub(crate) errors: Vec<SerializableValidationError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) load_error: Option<String>,
}

pub(crate) async fn get_config(
    State(state): State<WebState>,
) -> Result<Json<ConfigResponse>, WebError> {
    let config_path = &state.inner.config_path;
    let config_rx = state.inner.config_rx.borrow().clone();

    // Read raw TOML from disk.
    let raw_on_disk = std::fs::read_to_string(config_path)
        .map_err(|e| WebError::ConfigReadError(format!("cannot read config file: {e}")))?;

    // Re-validate the on-disk file.
    let creds_path = config_path.with_extension("creds.toml");
    let (warnings, errors, load_error) = match load_config(config_path, Strictness::Warn) {
        Ok((cfg, warns)) => {
            let creds = load_credentials(&creds_path).unwrap_or_default();
            let errs = validate(&cfg, &capabilities(), &creds);
            (warns, errs, None)
        }
        Err(e) => (vec![], vec![], Some(e.to_string())),
    };

    let ok = load_error.is_none() && errors.is_empty();

    // Source: "last_applied" when on-disk matches the running config;
    // otherwise "on_disk".
    let source = if load_error.is_none() {
        if let Ok((disk_cfg, _)) = load_config(config_path, Strictness::Warn) {
            if disk_cfg == *config_rx {
                "last_applied"
            } else {
                "on_disk"
            }
        } else {
            "on_disk"
        }
    } else {
        "on_disk"
    };

    // Secret redaction: strip inline userinfo from broker_url fields in inventory.
    let mut inventory = Arc::unwrap_or_clone(config_rx);
    redact_config_secrets(&mut inventory);

    // Secret redaction in raw TOML.
    let raw_toml = redact_raw_secrets(&raw_on_disk);

    let serializable_warnings: Vec<SerializableWarning> =
        warnings.iter().map(SerializableWarning::from).collect();
    let serializable_errors: Vec<SerializableValidationError> = errors
        .iter()
        .map(SerializableValidationError::from)
        .collect();

    Ok(Json(ConfigResponse {
        path: config_path.display().to_string(),
        config_version: inventory.config_version,
        source,
        raw_toml,
        inventory,
        validation: ConfigValidation {
            ok,
            warnings: serializable_warnings,
            errors: serializable_errors,
            load_error,
        },
    }))
}

// ── Secret redaction ─────────────────────────────────────────────────────────

/// Redact inline userinfo from every `broker_url` in a [`Config`].
fn redact_config_secrets(cfg: &mut Config) {
    for sensor in cfg.sensors.values_mut() {
        if let SensorConfig::Mqtt(MqttSensorCfg { broker_url, .. }) = sensor {
            *broker_url = redact_url(broker_url);
        }
    }
}

/// Redact inline userinfo in URL-shaped strings within raw TOML text.
///
/// Matches `scheme://user:pass@host` patterns and replaces userinfo with
/// `[redacted]`.
fn redact_raw_secrets(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            // Only redact lines that look like they contain a URL with userinfo.
            if line.contains("://") && line.contains('@') {
                redact_url(line)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip inline userinfo from a URL-shaped string.
///
/// `tcp://user:pass@host:1883` → `tcp://[redacted]@host:1883`
fn redact_url(s: &str) -> String {
    if let Some(scheme_end) = s.find("://") {
        let after_scheme = &s[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.find('@') {
            let userinfo = &after_scheme[..at_pos];
            // Only redact if no '/' between :// and @ (genuine URL userinfo,
            // not a path component containing @).
            if !userinfo.contains('/') {
                let scheme_part = &s[..=scheme_end + 2];
                let rest = &after_scheme[at_pos..]; // includes the @
                return format!("{scheme_part}[redacted]{rest}");
            }
        }
    }
    s.to_string()
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::schema::{DaemonConfig, SensorConfig};
    use indexmap::IndexMap;

    /// Write a config file to a temp dir for tests.
    fn write_temp_config(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// Build a minimal `WebState` for config route tests.
    fn test_config_state(config_path: std::path::PathBuf, config: Config) -> WebState {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(config));
        let creds = Arc::new(dormant_core::config::schema::Credentials::default());
        let (creds_tx, creds_rx) = tokio::sync::watch::channel(creds);
        let cancel = tokio_util::sync::CancellationToken::new();

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        WebState::new(super::super::super::state::WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path,
            doctor,
            web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            cancel,
        })
    }

    // ── Redaction unit tests ──────────────────────────────────────────────

    #[test]
    fn redact_url_strips_userinfo() {
        let result = redact_url("tcp://user:pass@host:1883");
        assert!(
            !result.contains("user"),
            "userinfo should be redacted: {result}"
        );
        assert!(
            !result.contains(":pass"),
            "password should be redacted: {result}"
        );
        assert!(
            result.contains("tcp://[redacted]@host:1883"),
            "unexpected: {result}"
        );
    }

    #[test]
    fn redact_url_preserves_no_userinfo() {
        let result = redact_url("tcp://host:1883");
        assert_eq!(result, "tcp://host:1883");
    }

    #[test]
    fn redact_url_does_not_touch_non_urls() {
        let result = redact_url("just plain text @ home");
        assert_eq!(result, "just plain text @ home");
    }

    #[test]
    fn redact_url_handles_at_in_path() {
        // @ after / in path → not userinfo, leave alone.
        let result = redact_url("https://example.com/path?email=user@host");
        assert_eq!(result, "https://example.com/path?email=user@host");
    }

    #[test]
    fn redact_config_secrets_redacts_broker_urls() {
        let mut cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: {
                let mut s = IndexMap::new();
                s.insert(
                    "desk".into(),
                    SensorConfig::Mqtt(MqttSensorCfg {
                        broker_url: "tcp://u:p@h:1883".into(),
                        topic: "test".into(),
                        field: "/val".into(),
                        payload_on: None,
                        payload_off: None,
                        kind: dormant_core::config::schema::SensorKind::default(),
                        hold_time: None,
                        stale_timeout: None,
                    }),
                );
                s
            },
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        };

        redact_config_secrets(&mut cfg);

        let SensorConfig::Mqtt(mqtt) = cfg.sensors.get("desk").unwrap() else {
            panic!("expected MQTT sensor");
        };
        assert!(
            !mqtt.broker_url.contains("u:p"),
            "secret should be redacted in broker_url"
        );
    }

    #[test]
    fn redact_raw_secrets_redacts_userinfo_in_toml() {
        let raw = r#"[sensors.desk]
type = "mqtt"
broker_url = "tcp://user:pass@host:1883"
topic = "test""#;
        let redacted = redact_raw_secrets(raw);
        assert!(
            !redacted.contains("user"),
            "user should be redacted: {redacted}"
        );
        assert!(
            !redacted.contains(":pass"),
            "pass should be redacted: {redacted}"
        );
        assert!(
            redacted.contains("[redacted]"),
            "should contain [redacted]: {redacted}"
        );
    }

    // ── Endpoint tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn config_endpoint_returns_200_and_inventory() {
        let (_dir, path) = write_temp_config(
            r"
config_version = 1
",
        );
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        };
        let state = test_config_state(path, cfg);
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;

        assert_eq!(resp.config_version, 1);
        assert!(!resp.raw_toml.is_empty());
        assert!(resp.validation.ok);
        assert!(resp.validation.errors.is_empty());
        assert!(resp.validation.load_error.is_none());
    }

    #[tokio::test]
    async fn config_endpoint_redacts_broker_url_secret() {
        let (_dir, path) = write_temp_config(
            r#"
config_version = 1

[sensors.desk]
type = "mqtt"
broker_url = "tcp://u:p@h:1883"
topic = "dormant/desk"
field = "/val"
"#,
        );
        // Build a matching config for the watch.
        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "desk".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://u:p@h:1883".into(),
                topic: "dormant/desk".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: dormant_core::config::schema::SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
            }),
        );
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        };
        let state = test_config_state(path, cfg);
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;

        // Check raw_toml has redacted userinfo.
        assert!(
            !resp.raw_toml.contains("u:p"),
            "raw_toml should not contain u:p: {}",
            resp.raw_toml
        );

        // Check inventory has redacted broker_url.
        let inventory_json = serde_json::to_string(&resp.inventory).unwrap();
        assert!(
            !inventory_json.contains("u:p"),
            "inventory should not contain u:p: {inventory_json}"
        );
    }
}
