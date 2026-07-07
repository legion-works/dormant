//! Configuration structs that mirror the dormant TOML file shape.
//!
//! Every public struct derives `Deserialize` (from TOML).  Key names are the
//! literal TOML keys — all `#[serde(rename = "...")]` annotations use grep-stable
//! string literals.  Defaults are pulled from [`super::defaults`] via
//! `#[serde(default = "...")]` function shims.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::defaults;
use crate::error::DormantError;
use crate::types::{BlankMode, LadderStage, SensorId, StageKind, ZoneId};
use crate::zone::{FusionMode, UnavailablePolicy, ZoneMember, ZoneSpec};

// ── Strictness / Warning / ValidationError ──────────────────────────────────────

/// How strictly unknown configuration keys should be handled.
///
/// Resolved at the CLI layer — this is NOT a TOML key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// Fail on the first unknown key.
    Strict,
    /// Collect unknown keys as warnings and continue.
    Warn,
}

/// A non-fatal configuration warning.
#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    /// Dot-separated path to the problematic key.
    pub key_path: String,
    /// Human-readable description.
    pub message: String,
}

/// A validation problem discovered by cross-reference checking.
///
/// Displayed as `"config error [{what}]: {detail}"`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    /// Short category (e.g. "missing credential").
    pub what: String,
    /// Full description.
    pub detail: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "config error [{}]: {}", self.what, self.detail)
    }
}

// ── Top-level Config ────────────────────────────────────────────────────────────

/// The root configuration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version — must be `1` for this release of dormant.
    pub config_version: u32,

    /// Daemon-level settings.
    #[serde(default)]
    pub daemon: DaemonConfig,

    /// Sensor definitions keyed by user-chosen id.
    #[serde(default)]
    pub sensors: IndexMap<String, SensorConfig>,

    /// Zone definitions keyed by user-chosen id.
    #[serde(default)]
    pub zones: IndexMap<String, ZoneConfig>,

    /// Display definitions keyed by user-chosen id.
    #[serde(default)]
    pub displays: IndexMap<String, DisplayConfig>,

    /// Rule definitions keyed by user-chosen id.
    #[serde(default)]
    pub rules: IndexMap<String, RuleConfig>,
}

// ── DaemonConfig ────────────────────────────────────────────────────────────────

/// Daemon-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// How long to wait after startup before acting.
    #[serde(default = "default_startup_holdoff", with = "humantime_serde")]
    pub startup_holdoff: Duration,

    /// How long a sensor can go silent before considered stale.
    #[serde(default = "default_stale_sensor_timeout", with = "humantime_serde")]
    pub stale_sensor_timeout: Duration,

    /// Log level for the daemon.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Path to the Unix-domain socket for `dormantctl` communication.
    pub socket_path: Option<PathBuf>,

    /// How to interpret the raw value returned by the screensaver `DBus`
    /// `GetSessionIdleTime` method (`"auto"` | `"ms"` | `"s"`).
    #[serde(default)]
    pub idle_time_unit: IdleTimeUnit,

    /// Which idle source to use for the activity inhibitor (`"auto"` | `"wayland"` |
    /// `"dbus"`). `"auto"` (default) picks Wayland when `WAYLAND_DISPLAY` is set
    /// and the compositor advertises `ext_idle_notifier_v1`, falling back to `DBus`.
    #[serde(default)]
    pub idle_source: IdleSource,

    /// Debounce window coalescing rapid config-file changes into one reload.
    #[serde(default = "default_reload_debounce", with = "humantime_serde")]
    pub reload_debounce: Duration,

    /// TCP port for the M2 web UI. `None` disables the web UI even when
    /// compiled with `--features web-ui`.
    #[serde(default)]
    pub web_port: Option<u16>,

    /// Bind address for the web UI. Defaults to loopback; a non-loopback
    /// value requires `web_allow_nonloopback`.
    #[serde(default = "default_web_bind")]
    pub web_bind: std::net::IpAddr,

    /// Opt-in to bind the web UI on a non-loopback address (widens the
    /// unauthenticated surface — see spec §8).
    #[serde(default)]
    pub web_allow_nonloopback: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            startup_holdoff: defaults::STARTUP_HOLDOFF,
            stale_sensor_timeout: defaults::STALE_SENSOR_TIMEOUT,
            log_level: defaults::LOG_LEVEL.into(),
            socket_path: None,
            idle_time_unit: IdleTimeUnit::default(),
            idle_source: IdleSource::default(),
            reload_debounce: defaults::RELOAD_DEBOUNCE,
            web_port: None,
            web_bind: defaults::WEB_BIND_DEFAULT,
            web_allow_nonloopback: false,
        }
    }
}

/// How to interpret the raw idle value returned by the screensaver `DBus`
/// `GetSessionIdleTime` method. Implementations disagree: the freedesktop
/// `ScreenSaver` XML contract says seconds, while current KDE `kscreenlocker`
/// (backed by `KIdleTime`) returns milliseconds. `Auto` detects the unit at
/// runtime from the delta between two polls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleTimeUnit {
    /// Detect the unit at runtime from consecutive-poll deltas.
    #[default]
    Auto,
    /// The raw value is milliseconds.
    Ms,
    /// The raw value is seconds.
    #[serde(rename = "s")]
    Secs,
}

/// Which idle source the activity inhibitor should use.
///
/// * `Auto` (default) — prefer Wayland's `ext_idle_notifier_v1` when
///   `WAYLAND_DISPLAY` is set and the compositor advertises the protocol,
///   fall back to `DBus` `GetSessionIdleTime` otherwise.
/// * `Wayland` — force the Wayland idle notifier; the daemon will error at
///   startup if the compositor does not expose the protocol.
/// * `Dbus` — always use the `DBus` screensaver poll.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleSource {
    /// Auto-detect: Wayland when available, else `DBus`.
    #[default]
    Auto,
    /// Force Wayland `ext_idle_notifier_v1`.
    Wayland,
    /// Force `DBus` screensaver poll.
    #[serde(rename = "dbus")]
    Dbus,
}

// ── SensorKind ──────────────────────────────────────────────────────────────────

/// What kind of sensing a sensor provides.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    /// Binary presence (room occupied / not).
    #[default]
    Presence,
    /// Motion detected (transient event).
    Motion,
}

// ── SensorConfig (internally-tagged enum) ───────────────────────────────────────

/// A sensor definition.  The `type` field discriminates which variant is used.
///
/// # Why not `#[serde(flatten)]` + `SensorCommon`?
///
/// TOML's internally-tagged enum (`tag = "type"`) does not compose well with
/// `#[serde(flatten)]` — serde can lose the tag when the flattened struct
/// shares key names with the variant.  To keep the TOML wire format clean
/// (identical key names, no nesting), the common fields (`kind`, `hold_time`,
/// `stale_timeout`) are inlined directly into each variant struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SensorConfig {
    /// MQTT sensor.
    #[serde(rename = "mqtt")]
    Mqtt(MqttSensorCfg),

    /// Home Assistant WebSocket sensor.
    #[serde(rename = "ha")]
    Ha(HaSensorCfg),

    /// USB-connected LD2410 mmWave radar.
    #[serde(rename = "usb-ld2410")]
    UsbLd2410(UsbLd2410Cfg),
}

impl SensorConfig {
    /// Return the sensor kind for this configuration.
    #[must_use]
    pub fn kind(&self) -> SensorKind {
        match self {
            Self::Mqtt(c) => c.kind,
            Self::Ha(c) => c.kind,
            Self::UsbLd2410(c) => c.kind,
        }
    }

    /// Return the per-sensor hold-time override, if any.
    #[must_use]
    pub fn hold_time(&self) -> Option<Duration> {
        match self {
            Self::Mqtt(c) => c.hold_time,
            Self::Ha(c) => c.hold_time,
            Self::UsbLd2410(c) => c.hold_time,
        }
    }

    /// Return the per-sensor stale-timeout override, if any.
    #[must_use]
    pub fn stale_timeout(&self) -> Option<Duration> {
        match self {
            Self::Mqtt(c) => c.stale_timeout,
            Self::Ha(c) => c.stale_timeout,
            Self::UsbLd2410(c) => c.stale_timeout,
        }
    }
}

// ── Per-variant sensor configs ──────────────────────────────────────────────────

/// Configuration for an MQTT-connected presence sensor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MqttSensorCfg {
    /// MQTT broker URL (e.g. `tcp://localhost:1883`).
    pub broker_url: String,

    /// MQTT topic to subscribe to.
    pub topic: String,

    /// JSON pointer into the MQTT payload (default `"/occupancy"`).
    #[serde(default = "default_mqtt_field")]
    pub field: String,

    /// Payload that indicates presence (optional — defaults to JSON `true`).
    pub payload_on: Option<String>,

    /// Payload that indicates absence (optional — defaults to JSON `false`).
    pub payload_off: Option<String>,

    // ── Inlined common fields ────────────────────────────────────────────────
    /// Sensor kind.
    #[serde(default)]
    pub kind: SensorKind,

    /// Per-sensor hold-time override.
    #[serde(default, with = "humantime_serde::option")]
    pub hold_time: Option<Duration>,

    /// Per-sensor stale-timeout override.
    #[serde(default, with = "humantime_serde::option")]
    pub stale_timeout: Option<Duration>,
}

/// Configuration for a Home Assistant WebSocket sensor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaSensorCfg {
    /// Home Assistant WebSocket URL.
    pub url: String,

    /// Home Assistant entity ID to track.
    pub entity: String,

    // ── Inlined common fields ────────────────────────────────────────────────
    /// Sensor kind.
    #[serde(default)]
    pub kind: SensorKind,

    /// Per-sensor hold-time override.
    #[serde(default, with = "humantime_serde::option")]
    pub hold_time: Option<Duration>,

    /// Per-sensor stale-timeout override.
    #[serde(default, with = "humantime_serde::option")]
    pub stale_timeout: Option<Duration>,
}

/// Configuration for a USB-connected LD2410 mmWave radar sensor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsbLd2410Cfg {
    /// Serial port path (e.g. `/dev/ttyUSB0`).
    pub port: String,

    /// Baud rate (default 256000).
    #[serde(default = "default_ld2410_baud")]
    pub baud: u32,

    // ── Inlined common fields ────────────────────────────────────────────────
    /// Sensor kind.
    #[serde(default)]
    pub kind: SensorKind,

    /// Per-sensor hold-time override.
    #[serde(default, with = "humantime_serde::option")]
    pub hold_time: Option<Duration>,

    /// Per-sensor stale-timeout override.
    #[serde(default, with = "humantime_serde::option")]
    pub stale_timeout: Option<Duration>,
}

// ── ZoneConfig ──────────────────────────────────────────────────────────────────

/// A zone definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneConfig {
    /// Fusion mode: `"any"`, `"all"`, `"quorum"`, or `"weighted"`.
    pub mode: String,

    /// Member identifiers.  Plain strings reference sensors; `"zone:<id>"`
    /// references a nested zone.
    pub members: Vec<String>,

    /// Required member count for `"quorum"` mode.
    pub quorum: Option<u32>,

    /// Threshold fraction (0.0–1.0) for `"weighted"` mode.
    pub threshold: Option<f32>,

    /// Per-member weights for `"weighted"` mode.
    #[serde(default)]
    pub weights: IndexMap<String, f32>,

    /// How to treat unavailable sensors.
    #[serde(default)]
    pub unavailable_policy: UnavailablePolicy,
}

impl ZoneConfig {
    /// Convert this config into a [`ZoneSpec`] for the fusion engine.
    ///
    /// # Errors
    ///
    /// - [`DormantError::ConfigInvalid`] if the mode string is unrecognized or
    ///   required parameters are missing (e.g. `"quorum"` without `quorum`).
    pub fn to_zone_spec(&self, id: &str) -> Result<ZoneSpec, DormantError> {
        let members: Result<Vec<ZoneMember>, DormantError> =
            self.members.iter().map(|m| parse_member(m)).collect();
        let members = members?;

        let mode = match self.mode.as_str() {
            "any" => FusionMode::Any,
            "all" => FusionMode::All,
            "quorum" => {
                let n = self.quorum.ok_or_else(|| DormantError::ConfigInvalid {
                    detail: format!("zone '{id}': mode 'quorum' requires the 'quorum' key"),
                })?;
                FusionMode::Quorum(n)
            }
            "weighted" => {
                let threshold = self.threshold.ok_or_else(|| DormantError::ConfigInvalid {
                    detail: format!("zone '{id}': mode 'weighted' requires the 'threshold' key"),
                })?;
                // Sanity checks on threshold.
                if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                    return Err(DormantError::ConfigInvalid {
                        detail: format!(
                            "zone '{id}': weighted threshold must be 0.0..=1.0, got {threshold}"
                        ),
                    });
                }
                FusionMode::Weighted { threshold }
            }
            other => {
                return Err(DormantError::ConfigInvalid {
                    detail: format!(
                        "zone '{id}': unknown fusion mode '{other}' (expected any|all|quorum|weighted)"
                    ),
                });
            }
        };

        // Check for extraneous keys: quorum set with non-quorum mode, threshold
        // set with non-weighted mode.
        if self.quorum.is_some() && self.mode != "quorum" {
            return Err(DormantError::ConfigInvalid {
                detail: format!("zone '{id}': 'quorum' key is only valid with mode 'quorum'"),
            });
        }
        if self.threshold.is_some() && self.mode != "weighted" {
            return Err(DormantError::ConfigInvalid {
                detail: format!("zone '{id}': 'threshold' key is only valid with mode 'weighted'"),
            });
        }

        let weights: HashMap<String, f32> =
            self.weights.iter().map(|(k, v)| (k.clone(), *v)).collect();

        Ok(ZoneSpec {
            id: ZoneId(id.into()),
            mode,
            members,
            weights,
            unavailable_policy: self.unavailable_policy,
        })
    }
}

/// Parse a member string into a [`ZoneMember`].
fn parse_member(raw: &str) -> Result<ZoneMember, DormantError> {
    if let Some(zone_id) = raw.strip_prefix("zone:") {
        if zone_id.is_empty() {
            return Err(DormantError::ConfigInvalid {
                detail: format!("zone member '{raw}': empty zone reference"),
            });
        }
        Ok(ZoneMember::Zone(ZoneId(zone_id.into())))
    } else {
        if raw.is_empty() {
            return Err(DormantError::ConfigInvalid {
                detail: "zone member is an empty string".into(),
            });
        }
        Ok(ZoneMember::Sensor(SensorId(raw.into())))
    }
}

// ── DisplayConfig ───────────────────────────────────────────────────────────────

/// A source for screensaver media: a local directory path, a set of URLs, or both.
///
/// Exactly one of `path` or non-empty `urls` must be set — both is a config error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreensaverSource {
    /// Local filesystem path to a directory of images or a playlist file.
    pub path: Option<String>,

    /// Remote URLs pointing to image or video files.
    #[serde(default)]
    pub urls: Vec<String>,

    /// Recurse into subdirectories when `path` is a directory.
    #[serde(default)]
    pub recurse: bool,

    /// Shuffle the order of source items (mutually exclusive with `order` —
    /// validation rejects a config that sets both).
    #[serde(default)]
    pub shuffle: bool,

    /// Explicit ordering strategy (`"sequential"`).
    /// Mutually exclusive with `shuffle`; validation rejects a config
    /// that sets both.
    pub order: Option<String>,

    /// How long each image is displayed before advancing.
    #[serde(default, with = "humantime_serde::option")]
    pub image_duration: Option<Duration>,
}

/// Configuration for the software screensaver overlay (render fallback).
///
/// Activated when a [`StageKind::RenderScreensaver`] ladder stage is reached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreensaverConfig {
    /// What event triggers the screensaver.  M1 only supports `"vacancy"`.
    #[serde(default = "default_trigger")]
    pub trigger: String,

    /// Whether the screensaver overlay should play audio.
    #[serde(default = "default_screensaver_audio")]
    pub audio: bool,

    /// Ordered list of media sources (files or URLs).
    #[serde(default)]
    pub source: Vec<ScreensaverSource>,

    /// How the screensaver player scales source frames onto the rendered
    /// output rectangle.  One of:
    ///
    /// - `"fill"` (default) — crop-to-fill; source is zoomed so the longer
    ///   axis covers the entire output, the off-axis is cropped.  No black
    ///   bars.  Matches the OS-screensaver norm (GNOME / KDE / Windows).
    /// - `"fit"` — aspect-fit letterbox; source is scaled to fit inside the
    ///   output while preserving aspect ratio; black bars fill the gap.
    ///   This was the legacy behaviour before the `scale_mode` key was
    ///   added.
    /// - `"stretch"` — the source is scaled to exactly fill the output,
    ///   distorting aspect ratio.  No black bars, but proportions may look
    ///   wrong; useful only when the source aspect matches the output
    ///   within a hair.
    /// - `"center"` — 1:1 centre; the source is shown at native pixel
    ///   dimensions (no scaling), centred in the output rectangle.  Black
    ///   bars fill the gap.
    ///
    /// `None` (the field absent from the TOML) is treated as `"fill"`.
    /// Validation rejects any other value with an `E_SCREENSAVER_SOURCE`-
    /// class error naming the allowed set.
    #[serde(default)]
    pub scale_mode: Option<String>,
}

/// A display definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Ordered list of controller names to try.
    pub controllers: Vec<String>,

    /// Primary blank mode to use.  Must be set unless `ladder` is provided.
    /// When `ladder` is present this field is ignored — the first
    /// `Controller(_)` stage in the ladder serves as the primary mode.
    #[serde(default)]
    pub blank_mode: Option<BlankMode>,

    /// Fallback blank mode if the primary is unsupported (best-effort).
    /// Cannot be set when `ladder` is present.
    #[serde(default)]
    pub degraded_mode: Option<BlankMode>,

    /// Ordered escalation ladder (replaces `blank_mode`).  Each rung is a
    /// [`LadderStage`]; the first `Controller(mode)` rung acts as the
    /// primary blank mode for the executor.
    #[serde(default)]
    pub ladder: Vec<LadderStage>,

    /// Software screensaver configuration, required when a ladder includes
    /// a [`StageKind::RenderScreensaver`] stage.
    #[serde(default)]
    pub screensaver: Option<ScreensaverConfig>,

    /// `KWin` output name (e.g. `"DP-1"`).
    pub output: Option<String>,

    /// DDC/CI display identifier.
    pub ddc_display: Option<String>,

    /// Hostname or IP for network-controllable displays (Samsung Tizen,
    /// LG webOS).
    pub host: Option<String>,

    /// MAC address for Wake-on-LAN displays.
    pub wol_mac: Option<String>,

    /// Shell command to blank the display.
    pub blank_command: Option<String>,

    /// Shell command to wake the display.
    pub wake_command: Option<String>,

    /// Supported blank modes for `"command"` / `"ha-passthrough"` controllers
    /// (the static capability set for these controllers is empty; the user
    /// declares supported modes here).
    pub modes: Option<Vec<BlankMode>>,

    /// Home Assistant URL for `"ha-passthrough"`.
    pub ha_url: Option<String>,

    /// HA service to call for blanking.
    pub blank_service: Option<String>,

    /// HA service data for blanking (arbitrary TOML value).
    pub blank_data: Option<toml::Value>,

    /// HA service to call for waking.
    pub wake_service: Option<String>,

    /// HA service data for waking (arbitrary TOML value).
    pub wake_data: Option<toml::Value>,

    /// Timeout for a single blank/wake command.
    #[serde(default = "default_command_timeout", with = "humantime_serde")]
    pub command_timeout: Duration,

    /// Brightness level to restore on wake (0–100).
    #[serde(default = "default_restore_brightness")]
    pub restore_brightness: u8,

    /// Treat an unreachable controller as if the display is blanked
    /// (fail-safe — don't leave a screen on when we can't reach it).
    #[serde(default = "default_treat_unreachable_as_blanked")]
    pub treat_unreachable_as_blanked: bool,
}

impl DisplayConfig {
    /// Return the normalised ladder: the user-supplied `ladder` if present,
    /// otherwise desugar `blank_mode` into a single-stage ladder.
    #[must_use]
    pub fn normalized_ladder(&self) -> Vec<LadderStage> {
        if !self.ladder.is_empty() {
            return self.ladder.clone();
        }
        let mode = self.blank_mode.unwrap_or(BlankMode::PowerOff);
        vec![LadderStage {
            kind: StageKind::Controller(mode),
            dwell: None,
        }]
    }

    /// The primary blank mode — the first `Controller(mode)` stage in the
    /// normalised ladder, or `PowerOff` if the ladder is render-only.
    ///
    /// Used by the executor and `DisplayRuntimeCfg` until Task 3 wires
    /// full ladder consumption.
    #[must_use]
    pub fn primary_blank_mode(&self) -> BlankMode {
        for stage in &self.normalized_ladder() {
            if let StageKind::Controller(m) = stage.kind {
                return m;
            }
        }
        BlankMode::PowerOff
    }

    /// True when this display is capable of software rendering: at least
    /// one controller is local (`kwin-dpms`, `ddcci`, or `command`) and
    /// the controller list is not composed SOLELY of remote controllers
    /// (`samsung-tizen`, `ha-passthrough`).
    #[must_use]
    pub fn is_render_eligible(&self) -> bool {
        let has_local = self
            .controllers
            .iter()
            .any(|c| matches!(c.as_str(), "kwin-dpms" | "ddcci" | "command"));
        let only_remote = self
            .controllers
            .iter()
            .all(|c| matches!(c.as_str(), "samsung-tizen" | "ha-passthrough"));
        has_local && !only_remote
    }
}

// ── RuleConfig ──────────────────────────────────────────────────────────────────

/// A rule that links a zone to one or more displays with timing parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleConfig {
    /// Zone whose presence drives blank/wake decisions.
    pub zone: String,

    /// Displays to control when this rule fires.
    pub displays: Vec<String>,

    /// Debounce period — zone must be stable for this long before acting.
    #[serde(default = "default_grace_period", with = "humantime_serde")]
    pub grace_period: Duration,

    /// Minimum time a display must stay blanked.
    #[serde(default = "default_min_blank_time", with = "humantime_serde")]
    pub min_blank_time: Duration,

    /// Minimum time a display must stay awake.
    #[serde(default = "default_min_wake_time", with = "humantime_serde")]
    pub min_wake_time: Duration,

    /// Named inhibitors that suppress this rule (M1: `"user-activity"`,
    /// `"manual-pause"`).
    #[serde(default)]
    pub inhibitors: Vec<String>,

    /// How long without input before user-activity inhibitor considers the
    /// user idle.
    #[serde(default = "default_activity_idle_threshold", with = "humantime_serde")]
    pub activity_idle_threshold: Duration,

    /// How often to poll activity state.
    #[serde(default = "default_activity_poll_interval", with = "humantime_serde")]
    pub activity_poll_interval: Duration,

    /// Number of retries for wake commands before escalating.
    #[serde(default = "default_wake_retries")]
    pub wake_retries: u32,

    /// Backoff before the first wake retry.
    #[serde(default = "default_wake_retry_backoff", with = "humantime_serde")]
    pub wake_retry_backoff: Duration,

    /// Interval between successive wake retries.
    #[serde(default = "default_wake_retry_interval", with = "humantime_serde")]
    pub wake_retry_interval: Duration,
}

// ── Credentials ─────────────────────────────────────────────────────────────────

/// Per-broker MQTT credentials (username + password).
///
/// Keyed by broker URL (the same string used in [`MqttSensorCfg::broker_url`]).
/// The password is redacted in [`Debug`] output.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MqttCredential {
    /// MQTT broker username.
    pub username: String,
    /// MQTT broker password — redacted in [`Debug`].
    pub password: String,
}

impl std::fmt::Debug for MqttCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttCredential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// External credentials loaded from a separate, permission-restricted file.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    /// Home Assistant long-lived access token.
    pub ha_token: Option<String>,

    /// Samsung TV tokens indexed by host (IP or hostname).
    #[serde(default)]
    pub samsung: IndexMap<String, String>,

    /// MQTT broker credentials indexed by broker URL.
    ///
    /// The key MUST be the **exact** `broker_url` string from the sensor's
    /// [`MqttSensorCfg`].  A `mqtt://host` vs `tcp://host` mismatch, or any
    /// trailing difference, silently misses the lookup — the sensor connects
    /// anonymously and auth will fail.
    #[serde(default)]
    pub mqtt: IndexMap<String, MqttCredential>,
}

// ── Serde default function shims ────────────────────────────────────────────────

// Each function exists only so serde can reference a function path in
// `#[serde(default = "...")]` — serde cannot call `defaults::CONST` directly.

fn default_startup_holdoff() -> Duration {
    defaults::STARTUP_HOLDOFF
}
fn default_stale_sensor_timeout() -> Duration {
    defaults::STALE_SENSOR_TIMEOUT
}
fn default_reload_debounce() -> Duration {
    defaults::RELOAD_DEBOUNCE
}
fn default_log_level() -> String {
    defaults::LOG_LEVEL.into()
}
fn default_mqtt_field() -> String {
    defaults::MQTT_FIELD.into()
}
fn default_ld2410_baud() -> u32 {
    defaults::LD2410_BAUD
}
fn default_command_timeout() -> Duration {
    defaults::COMMAND_TIMEOUT
}
fn default_restore_brightness() -> u8 {
    defaults::RESTORE_BRIGHTNESS
}
fn default_treat_unreachable_as_blanked() -> bool {
    true
}
fn default_grace_period() -> Duration {
    defaults::GRACE_PERIOD
}
fn default_min_blank_time() -> Duration {
    defaults::MIN_BLANK_TIME
}
fn default_min_wake_time() -> Duration {
    defaults::MIN_WAKE_TIME
}
fn default_activity_idle_threshold() -> Duration {
    defaults::ACTIVITY_IDLE_THRESHOLD
}
fn default_activity_poll_interval() -> Duration {
    defaults::ACTIVITY_POLL_INTERVAL
}
fn default_wake_retries() -> u32 {
    defaults::WAKE_RETRIES
}
fn default_wake_retry_backoff() -> Duration {
    defaults::WAKE_RETRY_BACKOFF
}
fn default_wake_retry_interval() -> Duration {
    defaults::WAKE_RETRY_INTERVAL
}
fn default_web_bind() -> std::net::IpAddr {
    defaults::WEB_BIND_DEFAULT
}
fn default_trigger() -> String {
    defaults::SCREENSAVER_TRIGGER.into()
}
fn default_screensaver_audio() -> bool {
    defaults::SCREENSAVER_AUDIO
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    // ── SensorConfig deserialization ──────────────────────────────────────

    #[test]
    fn deserialize_full_config() {
        let toml_str = include_str!("../../tests/fixtures/config/valid_full.toml");
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.config_version, 1);

        // Daemon defaults.
        assert_eq!(cfg.daemon.startup_holdoff, defaults::STARTUP_HOLDOFF);
        assert_eq!(
            cfg.daemon.stale_sensor_timeout,
            defaults::STALE_SENSOR_TIMEOUT
        );
        assert_eq!(cfg.daemon.log_level, "info");

        // Sensors.
        assert_eq!(cfg.sensors.len(), 3);
        let desk = &cfg.sensors["desk"];
        match desk {
            SensorConfig::Mqtt(c) => {
                assert_eq!(c.broker_url, "tcp://mqtt.local:1883");
                assert_eq!(c.topic, "sensors/desk");
                assert_eq!(c.field, "/occupancy"); // default
                assert_eq!(c.kind, SensorKind::Presence); // default
            }
            _ => panic!("expected Mqtt sensor"),
        }

        let couch = &cfg.sensors["couch"];
        match couch {
            SensorConfig::Ha(c) => {
                assert_eq!(c.url, "ws://ha.local:8123/api/websocket");
                assert_eq!(c.entity, "binary_sensor.couch_presence");
                assert_eq!(c.kind, SensorKind::Motion);
            }
            _ => panic!("expected Ha sensor"),
        }

        let radar = &cfg.sensors["radar"];
        match radar {
            SensorConfig::UsbLd2410(c) => {
                assert_eq!(c.port, "/dev/ttyUSB0");
                assert_eq!(c.baud, 256_000); // default
            }
            _ => panic!("expected UsbLd2410 sensor"),
        }

        // Zones.
        assert_eq!(cfg.zones.len(), 3);
        let office = &cfg.zones["office"];
        assert_eq!(office.mode, "any");
        assert_eq!(office.members, vec!["desk"]);
        assert_eq!(office.unavailable_policy, UnavailablePolicy::Present); // default

        let media = &cfg.zones["media"];
        assert_eq!(media.mode, "weighted");
        assert!((media.threshold.unwrap() - 0.6).abs() < f32::EPSILON);
        assert!((media.weights["couch"] - 2.0).abs() < f32::EPSILON);

        let nested = &cfg.zones["nested"];
        assert_eq!(nested.members, vec!["zone:office", "radar"]);

        // Displays.
        assert_eq!(cfg.displays.len(), 3);
        let main = &cfg.displays["main_monitor"];
        assert_eq!(main.controllers, vec!["kwin-dpms", "ddcci"]);
        assert_eq!(main.blank_mode, Some(BlankMode::PowerOff));
        assert_eq!(main.output.as_deref(), Some("DP-1"));
        assert_eq!(main.restore_brightness, 80); // default

        let tv = &cfg.displays["tv"];
        assert_eq!(tv.controllers, vec!["samsung-tizen"]);
        assert_eq!(tv.blank_mode, Some(BlankMode::ScreenOffAudioOn));
        assert_eq!(tv.host.as_deref(), Some("192.168.1.50"));

        let escape = &cfg.displays["escape"];
        assert_eq!(escape.controllers, vec!["command"]);
        assert_eq!(
            escape.blank_command.as_deref(),
            Some("/usr/bin/xset dpms force off")
        );
        assert_eq!(
            escape.wake_command.as_deref(),
            Some("/usr/bin/xset dpms force on")
        );
        assert_eq!(escape.modes.as_ref().unwrap(), &vec![BlankMode::PowerOff]);

        // Rules.
        assert_eq!(cfg.rules.len(), 2);
        let r1 = &cfg.rules["office_blank"];
        assert_eq!(r1.zone, "office");
        assert_eq!(r1.displays, vec!["main_monitor"]);
        assert_eq!(r1.grace_period, defaults::GRACE_PERIOD); // default

        let r2 = &cfg.rules["media_blank"];
        assert_eq!(r2.zone, "media");
        assert_eq!(r2.displays, vec!["tv", "escape"]);
        assert_eq!(r2.wake_retries, 5); // explicit in TOML
    }

    // ── ZoneConfig::to_zone_spec ──────────────────────────────────────────

    #[test]
    fn zone_config_to_spec_any() {
        let zc = ZoneConfig {
            mode: "any".into(),
            members: vec!["sensor_a".into(), "zone:child".into()],
            quorum: None,
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let spec = zc.to_zone_spec("test").unwrap();
        assert_eq!(spec.mode, FusionMode::Any);
        assert_eq!(spec.members.len(), 2);
        assert_eq!(
            spec.members[0],
            ZoneMember::Sensor(SensorId("sensor_a".into()))
        );
        assert_eq!(spec.members[1], ZoneMember::Zone(ZoneId("child".into())));
    }

    #[test]
    fn zone_config_to_spec_quorum_requires_quorum_key() {
        let zc = ZoneConfig {
            mode: "quorum".into(),
            members: vec!["a".into()],
            quorum: None,
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("requires the 'quorum' key"));
    }

    #[test]
    fn zone_config_to_spec_weighted_requires_threshold_key() {
        let zc = ZoneConfig {
            mode: "weighted".into(),
            members: vec!["a".into()],
            quorum: None,
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("requires the 'threshold' key"));
    }

    #[test]
    fn zone_config_unknown_mode_is_error() {
        let zc = ZoneConfig {
            mode: "fancy".into(),
            members: vec!["a".into()],
            quorum: None,
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("unknown fusion mode"));
    }

    #[test]
    fn zone_config_quorum_key_in_non_quorum_mode_is_error() {
        let zc = ZoneConfig {
            mode: "any".into(),
            members: vec!["a".into()],
            quorum: Some(3),
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("'quorum' key is only valid"));
    }

    #[test]
    fn zone_config_threshold_key_in_non_weighted_mode_is_error() {
        let zc = ZoneConfig {
            mode: "all".into(),
            members: vec!["a".into()],
            quorum: None,
            threshold: Some(0.5),
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("'threshold' key is only valid"));
    }

    #[test]
    fn zone_config_empty_zone_ref_is_error() {
        let zc = ZoneConfig {
            mode: "any".into(),
            members: vec!["zone:".into()],
            quorum: None,
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("empty zone reference"));
    }

    #[test]
    fn zone_config_empty_sensor_ref_is_error() {
        let zc = ZoneConfig {
            mode: "any".into(),
            members: vec![String::new()],
            quorum: None,
            threshold: None,
            weights: IndexMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        };
        let err = zc.to_zone_spec("test").unwrap_err();
        assert!(err.to_string().contains("empty string"));
    }

    // ── Strictness / Warning ──────────────────────────────────────────────

    #[test]
    fn strictness_is_copy() {
        let s = Strictness::Strict;
        let s2 = s;
        assert_eq!(s, s2);
    }

    #[test]
    fn validation_error_display_format() {
        let ve = ValidationError {
            what: "missing credential".into(),
            detail: "display 'tv' needs samsung token for host 192.168.1.50".into(),
        };
        let s = ve.to_string();
        assert!(s.starts_with("config error [missing credential]:"));
        assert!(s.contains("192.168.1.50"));
    }

    // ── ScreensaverConfig scale_mode ────────────────────────────────────────

    #[test]
    fn screensaver_scale_mode_absent_parses_as_none() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
audio = false
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let ss = cfg.displays["d1"].screensaver.as_ref().unwrap();
        assert!(
            ss.scale_mode.is_none(),
            "absent scale_mode must parse as None (default-fill), got {:?}",
            ss.scale_mode
        );
    }

    #[test]
    fn screensaver_scale_mode_each_valid_value_parses_as_some() {
        let valid = ["fill", "fit", "stretch", "center"];
        for v in valid {
            let toml_str = format!(
                r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
audio = false
scale_mode = "{v}"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#
            );
            let cfg: Config = toml::from_str(&toml_str)
                .unwrap_or_else(|e| panic!("scale_mode = '{v}' must parse: {e}"));
            let ss = cfg.displays["d1"].screensaver.as_ref().unwrap();
            assert_eq!(
                ss.scale_mode.as_deref(),
                Some(v),
                "scale_mode {v} must round-trip as Some({v:?}), got {:?}",
                ss.scale_mode
            );
        }
    }

    // ── Web-UI config keys ──────────────────────────────────────────────────

    #[test]
    fn daemon_web_keys_parse_with_defaults() {
        let cfg: Config = toml::from_str("config_version = 1\n[daemon]\n").unwrap();
        assert_eq!(cfg.daemon.web_port, None);
        assert_eq!(cfg.daemon.web_bind, std::net::IpAddr::from([127, 0, 0, 1]));
        assert!(!cfg.daemon.web_allow_nonloopback);
    }

    #[test]
    fn daemon_web_keys_parse_explicit() {
        let cfg: Config = toml::from_str(
            "config_version = 1\n[daemon]\nweb_port = 8080\nweb_bind = \"127.0.0.1\"\n",
        )
        .unwrap();
        assert_eq!(cfg.daemon.web_port, Some(8080));
    }

    // ── MqttCredential Debug redaction ─────────────────────────────────────

    #[test]
    fn mqtt_credential_debug_redacts_password() {
        let cred = MqttCredential {
            username: "alice".into(),
            password: "s3cret!".into(),
        };
        let debug_str = format!("{cred:?}");
        // Username must appear.
        assert!(
            debug_str.contains("alice"),
            "Debug should show username: {debug_str}"
        );
        // Password must NOT appear.
        assert!(
            !debug_str.contains("s3cret"),
            "Debug MUST NOT leak password: {debug_str}"
        );
        // Redacted marker must be present.
        assert!(
            debug_str.contains("<redacted>"),
            "Debug should show <redacted> marker: {debug_str}"
        );
    }
}
