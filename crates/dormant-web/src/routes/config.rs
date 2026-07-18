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
use sha2::{Digest, Sha256};

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
    /// Lowercase hex SHA-256 of the on-disk config bytes, computed before redaction.
    pub(crate) fingerprint: String,
    /// TOML-key paths of every value that was redacted, in discovery order.
    /// Array indices are rendered as decimal strings.
    pub(crate) redacted_paths: Vec<Vec<String>>,
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
    let redacted_paths = redact_config_secrets(&mut inventory);
    let display_rules = build_display_rules(&inventory);

    // ── Step 2 — on-disk validation (raw TOML + load/parse/creds) ──────
    let creds_path = &state.inner.creds_path;

    let raw_bytes = match std::fs::read(config_path) {
        Ok(bytes) => bytes,
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
                fingerprint: String::new(),
                redacted_paths,
            }));
        }
    };

    let fingerprint = format!("{:x}", Sha256::digest(&raw_bytes));
    let raw_on_disk = String::from_utf8_lossy(&raw_bytes).into_owned();

    let (warnings, errors, load_error) = match load_config(config_path, Strictness::Warn) {
        Ok((cfg, warns)) => {
            let creds = match load_credentials(creds_path) {
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
                        fingerprint,
                        redacted_paths,
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
        fingerprint,
        redacted_paths,
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
/// configs and display configs, returning the TOML-key path of each redacted
/// value in discovery order.
pub(super) fn redact_config_secrets(cfg: &mut Config) -> Vec<Vec<String>> {
    let mut paths: Vec<Vec<String>> = Vec::new();

    for (sensor_id, sensor) in &mut cfg.sensors {
        match sensor {
            SensorConfig::Mqtt(MqttSensorCfg { broker_url, .. }) => {
                let redacted = redact_url(broker_url);
                if redacted != *broker_url {
                    *broker_url = redacted;
                    paths.push(vec![
                        "sensors".to_string(),
                        sensor_id.clone(),
                        "broker_url".to_string(),
                    ]);
                }
            }
            SensorConfig::Ha(HaSensorCfg { url, .. }) => {
                let redacted = redact_url(url);
                if redacted != *url {
                    *url = redacted;
                    paths.push(vec![
                        "sensors".to_string(),
                        sensor_id.clone(),
                        "url".to_string(),
                    ]);
                }
            }
            SensorConfig::UsbLd2410(_) => {
                // No URL fields.
            }
        }
    }
    for (display_id, display) in &mut cfg.displays {
        redact_display_secrets(display, &mut paths, display_id);
    }
    paths
}

/// Redact userinfo in URL-shaped fields of a [`DisplayConfig`], appending each
/// redacted field's TOML-key path to `paths`.
fn redact_display_secrets(dc: &mut DisplayConfig, paths: &mut Vec<Vec<String>>, display_id: &str) {
    if let Some(ref url) = dc.ha_url {
        let redacted = redact_url(url);
        if redacted != *url {
            dc.ha_url = Some(redacted);
            paths.push(vec![
                "displays".to_string(),
                display_id.to_string(),
                "ha_url".to_string(),
            ]);
        }
    }
    // Screensaver source URLs — each is a URL-shaped string that may carry userinfo.
    if let Some(ref mut sc) = dc.screensaver {
        for (src_idx, source) in sc.source.iter_mut().enumerate() {
            for (url_idx, url) in source.urls.iter_mut().enumerate() {
                let redacted = redact_url(url);
                if redacted != *url {
                    *url = redacted;
                    paths.push(vec![
                        "displays".to_string(),
                        display_id.to_string(),
                        "screensaver".to_string(),
                        "source".to_string(),
                        src_idx.to_string(),
                        "urls".to_string(),
                        url_idx.to_string(),
                    ]);
                }
            }
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
    use dormant_core::config::defaults;
    use dormant_core::config::schema::{
        DaemonConfig, ScreensaverConfig, ScreensaverSource, SensorConfig,
    };
    use indexmap::IndexMap;
    use std::time::Duration;

    fn write_temp_config(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn test_config_state(config_path: std::path::PathBuf, config: Config) -> WebState {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) =
            tokio::sync::mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
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

        let creds_path = config_path.with_extension("creds.toml");

        WebState::new(super::super::super::state::WebStateInner::new_for_test(
            super::super::super::state::WebStateInnerParams {
                ctl_tx,
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path,
                creds_path,
                doctor,
                wear: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                web_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
                cancel,
                reload_timeout: Duration::from_secs(10),
            },
        ))
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
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
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
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
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
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
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
    fn redacted_paths_from_cfg_mqtt_and_screensaver_urls() {
        use dormant_core::config::schema::MqttSensorCfg;

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
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );

        let mut displays: IndexMap<String, DisplayConfig> = IndexMap::new();
        let source = ScreensaverSource {
            path: None,
            urls: vec!["http://user:pass@example.com/img.jpg".into()],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };
        let screensaver = ScreensaverConfig {
            trigger: "vacancy".into(),
            audio: false,
            source: vec![source],
            scale_mode: None,
            transition: None,
            transition_duration: None,
            shift_px: defaults::SHIFT_PX,
            shift_interval: defaults::SHIFT_INTERVAL,
        };
        displays.insert(
            "tv".into(),
            DisplayConfig {
                controllers: vec!["kwin-dpms".into()],
                blank_mode: None,
                degraded_mode: None,
                ladder: vec![],
                screensaver: Some(screensaver),
                output: None,
                ddc_display: None,
                host: None,
                wol_mac: None,
                blank_command: None,
                wake_command: None,
                modes: None,
                ha_url: None,
                blank_service: None,
                blank_data: None,
                wake_service: None,
                wake_data: None,
                command_timeout: std::time::Duration::from_secs(5),
                restore_brightness: 100,
                samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
                treat_unreachable_as_blanked: false,
                panel_type: dormant_core::wear::PanelType::default(),
            },
        );

        let mut cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays,
            rules: IndexMap::default(),
        };

        let paths = redact_config_secrets(&mut cfg);

        let expected: Vec<Vec<String>> = vec![
            vec!["sensors".into(), "desk".into(), "broker_url".into()],
            vec![
                "displays".into(),
                "tv".into(),
                "screensaver".into(),
                "source".into(),
                "0".into(),
                "urls".into(),
                "0".into(),
            ],
        ];
        assert_eq!(paths, expected, "redacted_paths must be exact");
    }

    #[test]
    fn redacted_paths_empty_when_no_secrets() {
        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert(
            "desk".into(),
            SensorConfig::Mqtt(MqttSensorCfg {
                broker_url: "tcp://host:1883".into(), // No userinfo
                topic: "dormant/desk".into(),
                field: "/val".into(),
                payload_on: None,
                payload_off: None,
                kind: dormant_core::config::schema::SensorKind::default(),
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }),
        );
        let mut cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors,
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
        };
        let paths = redact_config_secrets(&mut cfg);
        assert!(paths.is_empty());
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
    async fn fingerprint_is_sha256_of_disk_bytes() {
        let content = "config_version = 1\n";
        let (_dir, path) = write_temp_config(content);
        let state = test_config_state(path, config_with_secret());
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        let expected = format!("{:x}", Sha256::digest(content.as_bytes()));
        assert_eq!(resp.fingerprint, expected);
        // Fingerprint must be lowercase hex.
        assert!(
            resp.fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
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
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
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
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
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

    // ── Absent-optional serialization (#40) ────────────────────────────────

    /// A display with every display-level `Option` field absent, no ladder,
    /// no screensaver.
    fn display_all_optionals_absent(controller: &str) -> DisplayConfig {
        DisplayConfig {
            controllers: vec![controller.into()],
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: std::time::Duration::from_secs(5),
            restore_brightness: 100,
            samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: false,
            panel_type: dormant_core::wear::PanelType::default(),
        }
    }

    /// A display with a no-dwell ladder stage and a screensaver whose only
    /// source has `path`/`order`/`image_duration` absent; everything else
    /// display-level absent, per [`display_all_optionals_absent`].
    fn display_with_ladder_and_screensaver_source(controller: &str) -> DisplayConfig {
        use dormant_core::types::{LadderStage, StageKind};

        let source = ScreensaverSource {
            path: None,
            urls: vec!["http://example.com/img.jpg".into()],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };
        let screensaver = ScreensaverConfig {
            trigger: "vacancy".into(),
            audio: false,
            source: vec![source],
            scale_mode: None,
            transition: None,
            transition_duration: None,
            shift_px: defaults::SHIFT_PX,
            shift_interval: defaults::SHIFT_INTERVAL,
        };
        DisplayConfig {
            ladder: vec![LadderStage {
                kind: StageKind::RenderScreensaver,
                dwell: None,
            }],
            screensaver: Some(screensaver),
            ..display_all_optionals_absent(controller)
        }
    }

    fn config_with_displays(displays: IndexMap<String, DisplayConfig>) -> Config {
        Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: dormant_core::config::schema::WearConfig::default(),
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays,
            rules: IndexMap::default(),
        }
    }

    #[tokio::test]
    async fn config_endpoint_omits_absent_optional_fields_instead_of_null() {
        let mut displays: IndexMap<String, DisplayConfig> = IndexMap::new();
        displays.insert("tv".into(), display_all_optionals_absent("kwin-dpms"));
        displays.insert(
            "proj".into(),
            display_with_ladder_and_screensaver_source("command"),
        );

        let (_dir, path) = write_temp_config("config_version = 1\n");
        let state = test_config_state(path, config_with_displays(displays));
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;

        let inventory_value = serde_json::to_value(&resp.inventory).unwrap();
        let tv_obj = inventory_value["displays"]["tv"]
            .as_object()
            .expect("tv display must be an object");

        for key in [
            "blank_mode",
            "degraded_mode",
            "screensaver",
            "output",
            "ddc_display",
            "host",
            "wol_mac",
            "blank_command",
            "wake_command",
            "modes",
            "ha_url",
            "blank_service",
            "blank_data",
            "wake_service",
            "wake_data",
        ] {
            assert!(
                !tv_obj.contains_key(key),
                "expected key '{key}' to be ABSENT from displays.tv, but it is present as {:?}",
                tv_obj.get(key)
            );
        }

        let proj = &inventory_value["displays"]["proj"];
        let stage0_obj = proj["ladder"][0]
            .as_object()
            .expect("ladder stage must be an object");
        assert!(
            !stage0_obj.contains_key("dwell"),
            "expected key 'dwell' to be ABSENT from displays.proj.ladder[0], but it is present as {:?}",
            stage0_obj.get("dwell")
        );

        let source0_obj = proj["screensaver"]["source"][0]
            .as_object()
            .expect("source[0] must be an object");
        for key in ["path", "order", "image_duration"] {
            assert!(
                !source0_obj.contains_key(key),
                "expected key '{key}' to be ABSENT from displays.proj.screensaver.source[0], but it is present as {:?}",
                source0_obj.get(key)
            );
        }
    }

    #[tokio::test]
    async fn config_endpoint_round_trips_absent_optionals_through_json() {
        let original = display_with_ladder_and_screensaver_source("command");

        // Direct struct round-trip: the absent optionals must be omitted on
        // the wire (proven above) and still deserialize back to `None` —
        // this is additive omission, not a config-version change.
        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: DisplayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, original);
        assert_eq!(round_tripped.ladder[0].dwell, None);
        assert_eq!(
            round_tripped.screensaver.as_ref().unwrap().source[0].path,
            None
        );

        // Also exercise the whole-Config round trip via the endpoint path.
        let mut displays: IndexMap<String, DisplayConfig> = IndexMap::new();
        displays.insert("proj".into(), original);
        let (_dir, path) = write_temp_config("config_version = 1\n");
        let state = test_config_state(path, config_with_displays(displays));
        let result = get_config(State(state)).await.unwrap();
        let resp = result.0;
        let inventory_json = serde_json::to_string(&resp.inventory).unwrap();
        let round_tripped_inventory: Config = serde_json::from_str(&inventory_json).unwrap();
        let round_tripped_display = round_tripped_inventory.displays.get("proj").unwrap();
        assert_eq!(round_tripped_display.blank_mode, None);
        assert_eq!(round_tripped_display.output, None);
        assert_eq!(round_tripped_display.ladder[0].dwell, None);
        assert_eq!(
            round_tripped_display.screensaver.as_ref().unwrap().source[0].path,
            None
        );
    }
}
