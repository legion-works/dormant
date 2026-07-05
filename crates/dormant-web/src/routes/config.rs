//! `GET /api/config` — config inventory + raw TOML + validation.
//!
//! Returns the last-applied `Config` from the live watch together with the
//! on-disk raw TOML re-read at request time and its validation result.
//! Inline secrets (URL userinfo) are redacted in both `inventory` and
//! `raw_toml` before serving.  The response also includes the backend-side
//! display→zone→rule reverse-lookup (computed from `Config.rules`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use dormant_core::config::schema::{
    Config, DisplayConfig, HaSensorCfg, MqttSensorCfg, SensorConfig,
};
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

/// Per-display rule + zone mapping (reverse-lookup from `Config.rules`).
#[derive(Serialize, Debug)]
pub(crate) struct DisplayRuleInfo {
    pub(crate) rule: String,
    pub(crate) zone: String,
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
    /// Per-display → {rule, zone} reverse-lookup (spec §7 — backend-owned).
    pub(crate) display_rules: HashMap<String, DisplayRuleInfo>,
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

    // ── Step 1 — redacted inventory + display_rules from LAST-APPLIED ──
    // EVERY return path MUST use this redacted inventory so no secret leaks
    // on error paths.  Compute once up front.
    let mut inventory = Arc::unwrap_or_clone(config_rx);
    redact_config_secrets(&mut inventory);
    let display_rules = build_display_rules(&inventory);

    // ── Step 2 — on-disk validation (raw TOML + load/parse/creds) ──────
    let creds_path = config_path.with_extension("creds.toml");

    let raw_on_disk = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(e) => {
            // I/O read failure → normal body with load_error, redacted inventory.
            let raw_toml = redact_raw_secrets("");
            return Ok(Json(ConfigResponse {
                path: config_path.display().to_string(),
                config_version: inventory.config_version,
                source: "on_disk",
                raw_toml,
                inventory,
                validation: ConfigValidation {
                    ok: false,
                    warnings: vec![],
                    errors: vec![],
                    load_error: Some(format!("cannot read config file: {e}")),
                },
                display_rules,
            }));
        }
    };

    let (warnings, errors, load_error) = match load_config(config_path, Strictness::Warn) {
        Ok((cfg, warns)) => {
            let creds = match load_credentials(&creds_path) {
                Ok(c) => c,
                Err(e) => {
                    // Creds load failure → load_error, redacted inventory + raw_toml.
                    let raw_toml = redact_raw_secrets(&raw_on_disk);
                    return Ok(Json(ConfigResponse {
                        path: config_path.display().to_string(),
                        config_version: inventory.config_version,
                        source: "on_disk",
                        raw_toml,
                        inventory,
                        validation: ConfigValidation {
                            ok: false,
                            warnings: warns.iter().map(SerializableWarning::from).collect(),
                            errors: vec![],
                            load_error: Some(e.to_string()),
                        },
                        display_rules,
                    }));
                }
            };
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
            if disk_cfg == **state.inner.config_rx.borrow() {
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
        display_rules,
    }))
}

// ── Display → zone → rule reverse-lookup (spec §7) ─────────────────────────

/// Build a per-display mapping from `Config.rules`: for each display id,
/// record which rule drives it and which zone that rule references.
fn build_display_rules(cfg: &Config) -> HashMap<String, DisplayRuleInfo> {
    let mut map = HashMap::new();
    for (rule_id, rule_cfg) in &cfg.rules {
        for display_id in &rule_cfg.displays {
            map.entry(display_id.clone()).or_insert(DisplayRuleInfo {
                rule: rule_id.clone(),
                zone: rule_cfg.zone.clone(),
            });
        }
    }
    map
}

// ── Secret redaction ─────────────────────────────────────────────────────────

/// Redact inline userinfo from every URL-shaped string field across all sensor
/// configs and display configs.
fn redact_config_secrets(cfg: &mut Config) {
    for sensor in cfg.sensors.values_mut() {
        match sensor {
            SensorConfig::Mqtt(MqttSensorCfg { broker_url, .. }) => {
                *broker_url = redact_url(broker_url);
            }
            SensorConfig::Ha(HaSensorCfg { url, .. }) => {
                *url = redact_url(url);
            }
            SensorConfig::UsbLd2410(_) => {
                // No URL fields.
            }
        }
    }
    for display in cfg.displays.values_mut() {
        redact_display_secrets(display);
    }
}

/// Redact userinfo in URL-shaped fields of a [`DisplayConfig`].
fn redact_display_secrets(dc: &mut DisplayConfig) {
    if let Some(ref url) = dc.ha_url {
        let redacted = redact_url(url);
        if redacted != *url {
            dc.ha_url = Some(redacted);
        }
    }
    // host (IP/hostname) and wol_mac have no userinfo.
}

/// Redact inline userinfo in URL-shaped strings within raw TOML text.
///
/// Matches `scheme://user:pass@host` patterns and replaces userinfo with
/// `[redacted]`.
fn redact_raw_secrets(raw: &str) -> String {
    raw.lines()
        .map(|line| {
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
fn redact_url(s: &str) -> String {
    if let Some(scheme_end) = s.find("://") {
        let after_scheme = &s[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.find('@') {
            let userinfo = &after_scheme[..at_pos];
            if !userinfo.contains('/') {
                let scheme_part = &s[..=scheme_end + 2];
                let rest = &after_scheme[at_pos..];
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

    fn write_temp_config(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

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

    fn config_with_secret() -> Config {
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
        Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        }
    }

    // ── Display-rule reverse-lookup tests ──────────────────────────────────

    #[test]
    fn build_display_rules_maps_display_to_rule_and_zone() {
        let mut rules = IndexMap::new();
        rules.insert(
            "living_room".into(),
            dormant_core::config::schema::RuleConfig {
                zone: "living_zone".into(),
                displays: vec!["tv".into(), "monitor".into()],
                grace_period: std::time::Duration::from_secs(5),
                min_blank_time: std::time::Duration::from_secs(30),
                min_wake_time: std::time::Duration::from_secs(30),
                inhibitors: vec![],
                activity_idle_threshold: std::time::Duration::from_secs(300),
                activity_poll_interval: std::time::Duration::from_secs(30),
                wake_retries: 3,
                wake_retry_backoff: std::time::Duration::from_secs(2),
                wake_retry_interval: std::time::Duration::from_secs(2),
            },
        );
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules,
        };

        let map = build_display_rules(&cfg);
        assert_eq!(map.len(), 2);
        let tv = map.get("tv").unwrap();
        assert_eq!(tv.rule, "living_room");
        assert_eq!(tv.zone, "living_zone");
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
        assert_eq!(redact_url("tcp://host:1883"), "tcp://host:1883");
    }

    #[test]
    fn redact_url_does_not_touch_non_urls() {
        assert_eq!(
            redact_url("just plain text @ home"),
            "just plain text @ home"
        );
    }

    #[test]
    fn redact_url_handles_at_in_path() {
        assert_eq!(
            redact_url("https://example.com/path?email=user@host"),
            "https://example.com/path?email=user@host"
        );
    }

    #[test]
    fn redact_config_secrets_redacts_mqtt_broker_urls() {
        let mut cfg = config_with_secret();
        redact_config_secrets(&mut cfg);
        let SensorConfig::Mqtt(mqtt) = cfg.sensors.get("desk").unwrap() else {
            panic!("expected MQTT sensor");
        };
        assert!(!mqtt.broker_url.contains("u:p"));
    }

    #[test]
    fn redact_config_secrets_redacts_ha_sensor_urls() {
        let mut cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: {
                let mut s = IndexMap::new();
                s.insert(
                    "living".into(),
                    SensorConfig::Ha(HaSensorCfg {
                        url: "ws://sekrettoken@ha.local:8123/api/websocket".into(),
                        entity: "binary_sensor.motion".into(),
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
        let SensorConfig::Ha(ha) = cfg.sensors.get("living").unwrap() else {
            panic!("expected HA sensor");
        };
        assert!(
            !ha.url.contains("sekrettoken"),
            "HA url should be redacted: {}",
            ha.url
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
        let (_dir, path) = write_temp_config("config_version = 1\n");
        let state = test_config_state(path, config_with_secret());
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        assert_eq!(resp.config_version, 1);
        assert!(!resp.raw_toml.is_empty());
        assert!(resp.validation.ok);
        assert!(resp.validation.errors.is_empty());
        assert!(resp.validation.load_error.is_none());
    }

    #[tokio::test]
    async fn config_endpoint_redacts_mqtt_broker_url_secret() {
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
        let state = test_config_state(path, config_with_secret());
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        assert!(
            !resp.raw_toml.contains("u:p"),
            "raw_toml leaked secret: {}",
            resp.raw_toml
        );
        let inventory_json = serde_json::to_string(&resp.inventory).unwrap();
        assert!(
            !inventory_json.contains("u:p"),
            "inventory leaked secret: {inventory_json}"
        );
    }

    #[tokio::test]
    async fn config_endpoint_redacts_ha_sensor_url_secret() {
        let (_dir, path) = write_temp_config(
            r#"
config_version = 1
[sensors.living]
type = "ha"
url = "ws://sekrettoken@ha.local:8123/api/websocket"
entity = "binary_sensor.motion"
"#,
        );
        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "living".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://sekrettoken@ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.motion".into(),
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
        assert!(!resp.raw_toml.contains("sekrettoken"));
        let inventory_json = serde_json::to_string(&resp.inventory).unwrap();
        assert!(!inventory_json.contains("sekrettoken"));
    }

    #[tokio::test]
    async fn config_endpoint_includes_display_rules() {
        let (_dir, path) = write_temp_config("config_version = 1\n");
        let mut rules = IndexMap::new();
        rules.insert(
            "living_room".into(),
            dormant_core::config::schema::RuleConfig {
                zone: "living".into(),
                displays: vec!["tv".into()],
                grace_period: std::time::Duration::from_secs(5),
                min_blank_time: std::time::Duration::from_secs(30),
                min_wake_time: std::time::Duration::from_secs(30),
                inhibitors: vec![],
                activity_idle_threshold: std::time::Duration::from_secs(300),
                activity_poll_interval: std::time::Duration::from_secs(30),
                wake_retries: 3,
                wake_retry_backoff: std::time::Duration::from_secs(2),
                wake_retry_interval: std::time::Duration::from_secs(2),
            },
        );
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules,
        };
        let state = test_config_state(path, cfg);
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        let tv = resp
            .display_rules
            .get("tv")
            .expect("display_rules should include tv");
        assert_eq!(tv.rule, "living_room");
        assert_eq!(tv.zone, "living");
    }

    #[tokio::test]
    async fn config_endpoint_returns_load_error_on_unreadable_file_and_redacts() {
        let path = std::path::PathBuf::from("/nonexistent/config.toml");
        let state = test_config_state(path, config_with_secret());
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        // Must return load_error, not 500.
        assert!(!resp.validation.ok);
        assert!(resp.validation.load_error.is_some());
        // Redaction must still apply — no secret in inventory.
        let inventory_json = serde_json::to_string(&resp.inventory).unwrap();
        assert!(
            !inventory_json.contains("u:p"),
            "inventory leaked secret on error path: {inventory_json}"
        );
        // display_rules must be coherent (from last-applied inventory, not disk).
        assert!(resp.display_rules.is_empty());
    }

    #[tokio::test]
    async fn config_endpoint_redacts_on_creds_load_error() {
        // Use a temp dir with a valid config but an unreadable creds file
        // (wrong permissions on Unix, or a syntax error in creds).
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
config_version = 1
[sensors.desk]
type = "mqtt"
broker_url = "tcp://u:p@h:1883"
topic = "test"
field = "/val"
"#,
        )
        .unwrap();
        // Create a creds file with invalid TOML syntax so load_credentials fails.
        let creds_path = dir.path().join("config.creds.toml");
        std::fs::write(&creds_path, "{{{ not valid toml").unwrap();
        // On Unix, the permissions check might also fail.  Either way,
        // load_credentials returns an error.

        let state = test_config_state(config_path, config_with_secret());
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        // Must have load_error from creds failure.
        assert!(!resp.validation.ok);
        assert!(
            resp.validation.load_error.is_some(),
            "expected load_error on creds failure"
        );
        // Redaction must apply — no secret in inventory.
        let inventory_json = serde_json::to_string(&resp.inventory).unwrap();
        assert!(
            !inventory_json.contains("u:p"),
            "inventory leaked secret on creds error: {inventory_json}"
        );
        // raw_toml must also be redacted.
        assert!(
            !resp.raw_toml.contains("u:p"),
            "raw_toml leaked secret on creds error: {}",
            resp.raw_toml
        );
        // display_rules must be from last-applied (coherent with inventory).
        assert!(resp.display_rules.is_empty());
    }
}
