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
use crate::wear::PanelType;
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

    /// Panel-wear tracking configuration.
    #[serde(default)]
    pub wear: WearConfig,

    /// Wake-failure notification configuration.
    #[serde(default)]
    pub notifications: NotificationsConfig,

    /// Crash-loop watchdog / last-known-good rollback configuration.
    #[serde(default)]
    pub watchdog: WatchdogConfig,
    /// Global `PipeWire` audio-inhibitor configuration.
    #[serde(default)]
    pub audio: AudioConfig,

    /// Multi-machine coordination configuration.
    #[serde(default)]
    pub coordination: CoordinationConfig,

    /// Global KVM claim hotkey settings.
    #[serde(default)]
    pub keymap: KeymapConfig,

    /// Local input-device filtering for activity claims.
    #[serde(default)]
    pub input_filter: InputFilterConfig,

    /// Opt-in MQTT state publishing (issue #105).
    #[serde(default)]
    pub publish: PublishConfig,
}

/// Global KVM claim hotkey settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapConfig {
    /// Accelerator registered by the tray, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_hotkey: Option<String>,
}

/// Input-device filtering settings for activity claims.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputFilterConfig {
    /// Case-insensitive device-name globs that do not count as local activity.
    #[serde(default)]
    pub ignore_devices: Vec<String>,
}

/// Multi-machine coordination settings — direct local input writes
/// replace the earlier owner-mediated claim protocol (mDNS, SPAKE2,
/// Ed25519, TCP transport). The remaining fields govern ownership polls
/// and activity-follow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinationConfig {
    /// Interval between shared-display ownership polls.
    #[serde(
        default = "default_coordination_poll_interval",
        with = "humantime_serde"
    )]
    pub poll_interval: Duration,

    /// Interval between shared-display panel-state refreshes (brightness/power
    /// for `DisplaySnapshot` cosmetics); slower than `poll_interval` to cut
    /// per-transaction i2c traffic. When unset, the effective cadence is the
    /// larger of [`defaults::COORDINATION_STATE_POLL_INTERVAL`] and
    /// `poll_interval` via [`Self::effective_state_poll_interval`]; when set,
    /// must be `>= poll_interval` (see [`mod@super::validate`]). The rationale
    /// for the slower cadence lives on [`defaults::COORDINATION_STATE_POLL_INTERVAL`].
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_poll_interval: Option<Duration>,

    /// Number of consecutive agreeing "not mine" VCP `0x60` readings required
    /// before the cached ownership verdict flips `true → false` (issue #134).
    ///
    /// Defends against garbled reads from concurrent cross-machine DDC
    /// traffic on a shared panel: a single corrupted read should not blank a
    /// panel that's still ours. Ownership **gain** (`false → true`) stays
    /// eager — a possibly-wrong "I own" read triggers an idempotent wake that
    /// the next poll re-confirms. See
    /// [`defaults::COORDINATION_LOSS_CONFIRMATIONS`] for the default and the
    /// floor / ceiling rationale; validated to `1..=10` in
    /// [`mod@super::validate`].
    ///
    /// **Latency.** With defaults (`poll_interval = 2s`,
    /// `loss_confirmations = 3`), a genuine input switch takes ~6 seconds to
    /// commit. During that window the old owner still believes it owns the
    /// panel and may issue a blank; the new owner reads "mine" on its next
    /// poll and wakes eagerly. Lowering `loss_confirmations` toward `1`
    /// shortens that window at the cost of flap-susceptibility; raising it
    /// widens the genuine-handoff latency proportionally.
    ///
    /// **Limits.** The debounce reduces but does not eliminate false losses.
    /// If cross-machine DDC collisions return the same wrong code N times in
    /// a row, a false loss can still commit — N=3 makes a false loss ~3×
    /// less likely than N=1, not impossible. The `coord_poll_disagreement`
    /// signal only fires when consecutive observations *differ*; identical
    /// wrong readings sail through the debounce as if they were genuine.
    #[serde(default = "default_coordination_loss_confirmations")]
    pub loss_confirmations: u32,

    /// Whether local activity edges automatically pull a shared display.
    #[serde(default = "default_activity_follow")]
    pub activity_follow: bool,

    /// Grace window after a local-activity edge before the pull commits.
    #[serde(default = "default_arm_after", with = "humantime_serde")]
    pub arm_after: Duration,

    /// Minimum interval between successive activity-driven pulls.
    #[serde(default = "default_cooldown", with = "humantime_serde")]
    pub cooldown: Duration,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            poll_interval: defaults::COORDINATION_POLL_INTERVAL,
            state_poll_interval: None,
            loss_confirmations: defaults::COORDINATION_LOSS_CONFIRMATIONS,
            activity_follow: defaults::ACTIVITY_FOLLOW,
            arm_after: defaults::ARM_AFTER,
            cooldown: defaults::COOLDOWN,
        }
    }
}

impl CoordinationConfig {
    /// Effective panel-state refresh cadence: the explicit `state_poll_interval`
    /// when set, otherwise the larger of [`defaults::COORDINATION_STATE_POLL_INTERVAL`]
    /// and `poll_interval`, so the default is never below the ownership-poll cadence.
    #[must_use]
    pub fn effective_state_poll_interval(&self) -> Duration {
        self.state_poll_interval.unwrap_or_else(|| {
            self.poll_interval
                .max(defaults::COORDINATION_STATE_POLL_INTERVAL)
        })
    }
}

// ── DaemonConfig ────────────────────────────────────────────────────────────────

/// Daemon-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
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

    /// Maximum time a generation swap waits for its old engine to acknowledge
    /// the drained-input barrier. Validated to `> 0`.
    #[serde(
        default = "default_generation_barrier_ack_timeout",
        with = "humantime_serde"
    )]
    pub generation_barrier_ack_timeout: Duration,

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

    /// Whether the web UI's entity create/delete affordances are enabled.
    #[serde(default = "default_entity_crud_enabled")]
    pub entity_crud_enabled: bool,

    /// Whether the Samsung pairing wizard route is enabled.
    #[serde(default = "default_pairing_enabled")]
    pub pairing_enabled: bool,

    /// Whether the web UI's hook-edit affordance is enabled.
    /// Defaults to `false` — no argv-writing form on an unauthenticated
    /// loopback surface without explicit operator consent.
    #[serde(default)]
    pub hook_edit_enabled: bool,

    /// Timeout for a single pairing-wizard attempt. Validated to
    /// `30s..=300s`.
    #[serde(default = "default_pair_timeout", with = "humantime_serde")]
    pub pair_timeout: Duration,

    /// Post-wake settle window for the doctor exercise's bounded retry
    /// read (`dormantctl doctor --exercise`). Validated to `100ms..=30s`.
    #[serde(default = "default_doctor_wake_settle", with = "humantime_serde")]
    pub doctor_wake_settle: Duration,

    /// macOS `CoreGraphics` idle source: number of consecutive
    /// identical-looking polls before a reading is treated as "frozen"
    /// rather than genuine sustained idleness. Only consulted when
    /// `idle_source = "macos"`. Validated to `>= 2`.
    #[serde(default = "default_macos_idle_frozen_polls")]
    pub macos_idle_frozen_polls: u32,

    /// macOS `CoreGraphics` idle source: upper sanity bound on a single
    /// idle-time reading — readings above this are treated as bogus rather
    /// than genuine idle duration. Only consulted when
    /// `idle_source = "macos"`. Validated to `> 0`.
    #[serde(default = "default_macos_idle_sanity_cap", with = "humantime_serde")]
    pub macos_idle_sanity_cap: Duration,

    /// macOS `CoreGraphics` idle source: grace window after daemon startup
    /// before idle readings are trusted. Only consulted when
    /// `idle_source = "macos"`. Validated to `> 0`.
    #[serde(default = "default_macos_idle_startup_grace", with = "humantime_serde")]
    pub macos_idle_startup_grace: Duration,
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
            generation_barrier_ack_timeout: defaults::GENERATION_BARRIER_ACK_TIMEOUT,
            web_port: None,
            web_bind: defaults::WEB_BIND_DEFAULT,
            web_allow_nonloopback: false,
            entity_crud_enabled: defaults::ENTITY_CRUD_ENABLED,
            pairing_enabled: defaults::PAIRING_ENABLED,
            pair_timeout: defaults::PAIR_TIMEOUT,
            hook_edit_enabled: defaults::HOOK_EDIT_ENABLED,
            doctor_wake_settle: defaults::DOCTOR_WAKE_SETTLE,
            macos_idle_frozen_polls: defaults::MACOS_IDLE_FROZEN_POLLS,
            macos_idle_sanity_cap: defaults::MACOS_IDLE_SANITY_CAP,
            macos_idle_startup_grace: defaults::MACOS_IDLE_STARTUP_GRACE,
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
/// * `Auto` (default) — on macOS, always use the `CoreGraphics` idle-time
///   source; on Linux, prefer Wayland's `ext_idle_notifier_v1` when
///   `WAYLAND_DISPLAY` is set and the compositor advertises the protocol,
///   falling back to `DBus` `GetSessionIdleTime` otherwise; elsewhere
///   (Windows), `DBus` (whose non-Linux implementation is an inert stub —
///   see `dormantd::idle_source`'s `dbus_run`).
/// * `Wayland` — force the Wayland idle notifier; the daemon will error at
///   startup if the compositor does not expose the protocol.
/// * `Dbus` — always use the `DBus` screensaver poll.
/// * `Macos` — use the macOS `CoreGraphics` idle-time source (see
///   [`super::defaults::MACOS_IDLE_FROZEN_POLLS`] and friends for the
///   accompanying `[daemon]` tunables). Additive: this enum only describes
///   what the TOML key accepts — selecting `macos` on a non-macOS build is
///   *not* rejected here or at startup. The wiring layer
///   (`dormantd::idle_source::create_source`) treats it the same way it
///   treats `wayland` requested on non-Linux: warn once
///   (`macos_idle_unsupported`) and fail toward inactive by falling back to
///   `DBus`, rather than refusing to start.
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
    /// Force the macOS `CoreGraphics` idle-time source.
    Macos,
}

// ── WearConfig ──────────────────────────────────────────────────────────────────

/// Panel-wear tracking configuration (the `[wear]` TOML section).
///
/// Governs the pure [`crate::wear::WearLedger`] sampling/persistence cadence
/// and the heuristics the tracker in `dormantd` uses to attribute
/// brightness-weighted on-time to grid cells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearConfig {
    /// Enable panel-wear tracking. Enabled by default.
    #[serde(default = "default_wear_enabled")]
    pub enabled: bool,

    /// How often to sample panel state for wear attribution.
    #[serde(default = "default_wear_sample_interval", with = "humantime_serde")]
    pub sample_interval: Duration,

    /// How often to persist the wear ledger to disk.
    #[serde(default = "default_wear_persist_interval", with = "humantime_serde")]
    pub persist_interval: Duration,

    /// Timeout for a single panel-state read during sampling.
    #[serde(default = "default_wear_read_timeout", with = "humantime_serde")]
    pub read_timeout: Duration,

    /// Number of rows in the wear-attribution grid.
    #[serde(default = "default_wear_grid_rows")]
    pub grid_rows: u16,

    /// Number of columns in the wear-attribution grid.
    #[serde(default = "default_wear_grid_cols")]
    pub grid_cols: u16,

    /// Brightness fraction (0.0–1.0) assumed when the real brightness can't
    /// be read from the panel.
    #[serde(default = "default_wear_fallback_brightness")]
    pub fallback_brightness: f64,

    /// Brightness fraction (0.0–1.0) attributed while the screensaver is
    /// active (screensavers typically dim the panel below full brightness).
    #[serde(default = "default_wear_screensaver_factor")]
    pub screensaver_factor: f64,

    /// Minimum dwell before a blank/wake cycle counts as a "full" cycle
    /// rather than a short cycle for wear-cycle-count heuristics.
    #[serde(default = "default_wear_short_cycle_dwell", with = "humantime_serde")]
    pub short_cycle_dwell: Duration,

    /// Panel age (accumulated on-hours) after which wear advisories start
    /// surfacing to the operator.
    #[serde(default = "default_wear_advisory_after", with = "humantime_serde")]
    pub advisory_after: Duration,

    /// Active-time spatial sampling configuration.
    #[serde(default)]
    pub active_sampling: ActiveSamplingConfig,
}

/// Active-time spatial wear sampling configuration.
///
/// `sampled_displays` is the canonical multi-display config field. The
/// legacy singular `sampled_display` remains accepted for backward
/// compatibility within `config_version = 1`; supplying both keys is a
/// parse error so the operator's intent is unambiguous (see
/// [`ActiveSamplingConfig::selected_displays`]).
///
/// Every id in [`ActiveSamplingConfig::selected_displays`] drives an
/// independent sampler with
/// its own consent record, capture stream, lifecycle status, and
/// cancellation token. [`ActiveSamplingConfig::first_sampled_display`]
/// remains as the single-display accessor for consumers that classify
/// one status at a time (the doctor probes).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActiveSamplingConfig {
    /// Enable active sampling.
    #[serde(default = "default_active_sampling_enabled")]
    pub enabled: bool,

    /// Legacy single-display selector. Mutually exclusive with
    /// `sampled_displays` at the wire — exactly one of the two keys may
    /// be present in a config.
    ///
    /// The `skip_serializing_if` keeps the singular form out of the
    /// wire when the canonical plural list is in use, so a config that
    /// was authored with `sampled_displays` round-trips to the same key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_display: Option<String>,

    /// Multi-display selector. The canonical field; the singular
    /// `sampled_display` legacy form is translated into a one-element
    /// list at deserialization time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sampled_displays: Vec<String>,

    /// `PipeWire` stream lifecycle strategy.
    #[serde(default = "default_active_sampling_stream_mode")]
    pub stream_mode: StreamMode,

    /// Maximum time spent capturing one sample.
    #[serde(
        default = "default_active_sampling_capture_timeout",
        with = "humantime_serde"
    )]
    pub capture_timeout: Duration,

    /// Consecutive failures before the circuit opens.
    #[serde(default = "default_active_sampling_failure_threshold")]
    pub failure_threshold: u32,

    /// Delay before retrying an open circuit.
    #[serde(
        default = "default_active_sampling_circuit_reset_after",
        with = "humantime_serde"
    )]
    pub circuit_reset_after: Duration,
}

impl Default for ActiveSamplingConfig {
    fn default() -> Self {
        Self {
            enabled: default_active_sampling_enabled(),
            sampled_display: None,
            sampled_displays: Vec::new(),
            stream_mode: default_active_sampling_stream_mode(),
            capture_timeout: default_active_sampling_capture_timeout(),
            failure_threshold: default_active_sampling_failure_threshold(),
            circuit_reset_after: default_active_sampling_circuit_reset_after(),
        }
    }
}

impl ActiveSamplingConfig {
    /// Returns the configured display ids in declaration order.
    ///
    /// This is the single source of truth for which displays are sampled.
    /// Legacy singular configs and the canonical plural list both surface
    /// here. Callers MUST go through this method rather than reading
    /// `sampled_display` directly, so the per-display path is consistent
    /// across the daemon.
    #[must_use]
    pub fn selected_displays(&self) -> Vec<String> {
        if !self.sampled_displays.is_empty() {
            return self.sampled_displays.clone();
        }
        match self.sampled_display.as_ref() {
            Some(display) => vec![display.clone()],
            None => Vec::new(),
        }
    }

    /// Returns the singular display id the single-display runtime path
    /// operates on, borrowing from either the legacy singular field or
    /// the first element of a 1-element plural list.
    ///
    /// The runtime's consent flow, daemon-lifetime sampler, and IPC
    /// `display_exists` check all predate the multi-display registry
    /// and read a single display id. A 1-element `sampled_displays` list
    /// therefore drives that runtime path so an operator who adopts the
    /// canonical plural form for forward-compat with multi-display
    /// sampling does not silently suspend on this branch. Lists with
    /// `> 1` entries return `None` and stay suspended until the
    /// multi-display registry lands.
    #[must_use]
    pub fn first_sampled_display(&self) -> Option<&str> {
        if let Some(display) = self.sampled_display.as_deref() {
            return Some(display);
        }
        match self.sampled_displays.first() {
            Some(display) if self.sampled_displays.len() == 1 => Some(display.as_str()),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ActiveSamplingConfig {
    #[allow(
        clippy::too_many_lines,
        reason = "each branch handles a distinct config key; the visitor pattern keeps the keys visible"
    )]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};
        use std::fmt;

        // The wire shape is a flat table. Each known key is optional;
        // its default matches the corresponding `#[serde(default = ...)]`
        // shim on the struct fields so an empty `{}` parses to the same
        // shape as the impl-Default values. `#[serde(other)]` on the
        // catch-all `Other` arm keeps the repo's two-stage discipline
        // intact: unknown keys are consumed here and the
        // `validate::collect_unknown_keys` walker surfaces them with
        // `E_CONFIG_UNKNOWN_KEY` rather than being rejected at parse.
        #[derive(Default, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Enabled,
            SampledDisplay,
            SampledDisplays,
            StreamMode,
            CaptureTimeout,
            FailureThreshold,
            CircuitResetAfter,
            #[serde(other)]
            #[default]
            Other,
        }

        struct ActiveSamplingConfigVisitor;

        impl<'de> Visitor<'de> for ActiveSamplingConfigVisitor {
            type Value = ActiveSamplingConfig;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("active sampling configuration")
            }

            fn visit_map<V>(self, mut map: V) -> Result<ActiveSamplingConfig, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut enabled: Option<bool> = None;
                let mut sampled_display: Option<Option<String>> = None;
                let mut sampled_displays: Option<Vec<String>> = None;
                let mut stream_mode: Option<StreamMode> = None;
                let mut capture_timeout: Option<Duration> = None;
                let mut failure_threshold: Option<u32> = None;
                let mut circuit_reset_after: Option<Duration> = None;

                while let Some(key) = map.next_key::<Field>()? {
                    match key {
                        Field::Enabled => {
                            if enabled.is_some() {
                                return Err(V::Error::duplicate_field("enabled"));
                            }
                            enabled = Some(map.next_value()?);
                        }
                        Field::SampledDisplay => {
                            if sampled_display.is_some() {
                                return Err(V::Error::duplicate_field("sampled_display"));
                            }
                            sampled_display = Some(map.next_value()?);
                        }
                        Field::SampledDisplays => {
                            if sampled_displays.is_some() {
                                return Err(V::Error::duplicate_field("sampled_displays"));
                            }
                            sampled_displays = Some(map.next_value()?);
                        }
                        Field::StreamMode => {
                            if stream_mode.is_some() {
                                return Err(V::Error::duplicate_field("stream_mode"));
                            }
                            stream_mode = Some(map.next_value()?);
                        }
                        Field::CaptureTimeout => {
                            if capture_timeout.is_some() {
                                return Err(V::Error::duplicate_field("capture_timeout"));
                            }
                            // humantime string → Duration; delegates to the
                            // humantime_serde adapter used by the previous
                            // serde-generated code so existing configs
                            // (`capture_timeout = "3s"`) keep parsing.
                            let raw: String = map.next_value()?;
                            capture_timeout =
                                Some(humantime::parse_duration(&raw).map_err(V::Error::custom)?);
                        }
                        Field::FailureThreshold => {
                            if failure_threshold.is_some() {
                                return Err(V::Error::duplicate_field("failure_threshold"));
                            }
                            failure_threshold = Some(map.next_value()?);
                        }
                        Field::CircuitResetAfter => {
                            if circuit_reset_after.is_some() {
                                return Err(V::Error::duplicate_field("circuit_reset_after"));
                            }
                            let raw: String = map.next_value()?;
                            circuit_reset_after =
                                Some(humantime::parse_duration(&raw).map_err(V::Error::custom)?);
                        }
                        Field::Other => {
                            // Unknown keys are surfaced by the unknown-key
                            // walker in `validate`; here we just consume
                            // the value to keep parsing forward.
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let sampled_display = sampled_display.unwrap_or(None);
                let sampled_displays = sampled_displays.unwrap_or_default();

                // Mutually exclusive: both keys present is an error so
                // the operator's intent is unambiguous (the plan trap).
                if sampled_display.is_some() && !sampled_displays.is_empty() {
                    return Err(V::Error::custom(
                        "wear.active_sampling.sampled_display and sampled_displays are mutually exclusive; supply only one",
                    ));
                }

                // Within a plural list, duplicates and sanitized-collision
                // ids would race for the same on-disk consent file; reject
                // both up front so the operator learns at config parse.
                let mut seen_raw: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut seen_sanitized: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for display in &sampled_displays {
                    if !seen_raw.insert(display.clone()) {
                        return Err(V::Error::custom(format!(
                            "wear.active_sampling.sampled_displays contains duplicate id '{display}'"
                        )));
                    }
                    let sanitized = crate::wear::sanitize_identity_key(display);
                    if !seen_sanitized.insert(sanitized.clone()) {
                        return Err(V::Error::custom(format!(
                            "wear.active_sampling.sampled_displays contains ids that sanitize to the same key '{sanitized}'; rename one to keep consent records distinct"
                        )));
                    }
                }

                Ok(ActiveSamplingConfig {
                    enabled: enabled.unwrap_or_else(default_active_sampling_enabled),
                    sampled_display,
                    sampled_displays,
                    stream_mode: stream_mode.unwrap_or_else(default_active_sampling_stream_mode),
                    capture_timeout: capture_timeout
                        .unwrap_or_else(default_active_sampling_capture_timeout),
                    failure_threshold: failure_threshold
                        .unwrap_or_else(default_active_sampling_failure_threshold),
                    circuit_reset_after: circuit_reset_after
                        .unwrap_or_else(default_active_sampling_circuit_reset_after),
                })
            }
        }

        deserializer.deserialize_map(ActiveSamplingConfigVisitor)
    }
}

/// Stream setup strategy for active sampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamMode {
    /// Keep the capture stream warm between samples.
    Warm,
    /// Set up and tear down the stream for each sample.
    PerTick,
}

impl Default for WearConfig {
    fn default() -> Self {
        Self {
            enabled: defaults::WEAR_ENABLED,
            sample_interval: defaults::WEAR_SAMPLE_INTERVAL,
            persist_interval: defaults::WEAR_PERSIST_INTERVAL,
            read_timeout: defaults::WEAR_READ_TIMEOUT,
            grid_rows: defaults::WEAR_GRID_ROWS,
            grid_cols: defaults::WEAR_GRID_COLS,
            fallback_brightness: defaults::WEAR_FALLBACK_BRIGHTNESS,
            screensaver_factor: defaults::WEAR_SCREENSAVER_FACTOR,
            short_cycle_dwell: defaults::WEAR_SHORT_CYCLE_DWELL,
            advisory_after: defaults::WEAR_ADVISORY_AFTER,
            active_sampling: ActiveSamplingConfig::default(),
        }
    }
}

// ── NotificationsConfig ─────────────────────────────────────────────────────────

/// Wake-failure notification configuration (the `[notifications]` TOML section).
///
/// Governs when the daemon surfaces a desktop notification after repeated
/// wake-command failures for a display, the cooldown between successive
/// notifications, and whether a recovery notification fires once the display
/// wakes successfully again.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NotificationsConfig {
    /// Enable wake-failure notifications. Enabled by default.
    #[serde(default = "default_notify_enabled")]
    pub enabled: bool,

    /// Number of consecutive wake-command failures before a notification fires.
    #[serde(default = "default_notify_wake_attempt_threshold")]
    pub wake_attempt_threshold: u64,

    /// Minimum time between successive wake-failure notifications for the
    /// same display.
    #[serde(default = "default_notify_cooldown", with = "humantime_serde")]
    pub cooldown: Duration,

    /// Emit a recovery notification when a previously-failing display wakes
    /// successfully again.
    #[serde(default = "default_notify_recovery")]
    pub notify_recovery: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: defaults::NOTIFY_ENABLED,
            wake_attempt_threshold: defaults::NOTIFY_WAKE_ATTEMPT_THRESHOLD,
            cooldown: defaults::NOTIFY_COOLDOWN,
            notify_recovery: defaults::NOTIFY_RECOVERY,
        }
    }
}

// ── WatchdogConfig ──────────────────────────────────────────────────────────────
/// Crash-loop watchdog / last-known-good rollback configuration (the
/// `[watchdog]` TOML section).
///
/// Governs whether the boot guard tracks a last-known-good (LKG) config
/// generation, whether a detected crash loop is allowed to trigger an
/// automatic rollback to that generation, and how long a boot must stay up
/// before it counts as "stable" for LKG purposes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Track a last-known-good config generation. Enabled by default.
    #[serde(default = "default_lkg_enabled")]
    pub lkg_enabled: bool,
    /// Allow a detected crash loop to trigger an automatic rollback to the
    /// last-known-good generation. Enabled by default.
    #[serde(default = "default_lkg_rollback_enabled")]
    pub lkg_rollback_enabled: bool,
    /// How long a boot must stay up before it counts as stable for LKG
    /// purposes.
    #[serde(default = "default_lkg_stability_window", with = "humantime_serde")]
    pub stability_window: Duration,
}
impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            lkg_enabled: defaults::LKG_ENABLED,
            lkg_rollback_enabled: defaults::LKG_ROLLBACK_ENABLED,
            stability_window: defaults::LKG_STABILITY_WINDOW,
        }
    }
}

// ── AudioConfig ─────────────────────────────────────────────────────────────────
/// Global `PipeWire` audio-inhibitor configuration (the `[audio]` TOML
/// section).
///
/// Global rather than per-rule (a deliberate divergence from the per-rule
/// `activity_*` precedent): stream classification is a system-wide fact
/// about ONE `PipeWire` instance — per-rule copies would be fake granularity
/// that still collapses to one poller. Rules choose WHICH kinds inhibit them
/// via `RuleConfig::inhibitors`; `[audio]` says what those kinds MEAN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioConfig {
    /// How often to poll `pw_dump_command` for the current `PipeWire` graph.
    #[serde(default = "default_audio_poll_interval", with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Minimum continuous stream activity before the audio inhibitor
    /// asserts (debounces transient blips such as a notification chime).
    /// Deassertion is immediate — the debounce is asymmetric on purpose,
    /// fail-toward-blanking.
    #[serde(default = "default_audio_min_active", with = "humantime_serde")]
    pub min_active: Duration,
    /// `media.role` values that mean "this running stream is a call".
    #[serde(default = "default_audio_call_roles")]
    pub call_roles: Vec<String>,
    /// Optional narrowing filter for the `"audio-playback"` kind: when set,
    /// only running output streams whose `media.role` is in this list
    /// inhibit. Unset (the default) means every non-call running output
    /// stream inhibits — the permissive default avoids a curated role list
    /// `PipeWire` apps notoriously fill inconsistently. An explicitly empty
    /// list is rejected by validation (it would silently disable playback
    /// inhibition).
    #[serde(default)]
    pub playback_roles: Option<Vec<String>>,
    /// Whether a running INPUT stream (an open microphone) counts as a
    /// call. Defaults to `false`: `PipeWire` input nodes commonly sit
    /// `running` for hours under ordinary setups (idling Discord/Teams,
    /// OBS mic sources, wake-word assistants, granted browser mic
    /// permissions) — a `true` default would silently defeat blanking for a
    /// wide slice of users. Opt in deliberately.
    #[serde(default)]
    pub capture_is_call: bool,
    /// Override for the `pw-dump` invocation. Split with
    /// `str::split_whitespace()` — no shell, no quoting support; paths
    /// containing spaces are unsupported. This is the test/override seam
    /// (point it at a fake script emitting fixture JSON).
    #[serde(default = "default_audio_pw_dump_command")]
    pub pw_dump_command: String,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            poll_interval: defaults::AUDIO_POLL_INTERVAL,
            min_active: defaults::AUDIO_MIN_ACTIVE,
            call_roles: default_audio_call_roles(),
            playback_roles: None,
            capture_is_call: false,
            pw_dump_command: defaults::AUDIO_PW_DUMP_COMMAND.into(),
        }
    }
}

// ── PublishConfig ────────────────────────────────────────────────────────────────

/// Opt-in state-publish configuration (the `[publish]` TOML section).
///
/// Implements issue #105. When `enabled` is `false` (the default), the
/// daemon spawns no publisher task and the broker / discovery / LWT
/// machinery is never started. When `enabled`, the section is required to
/// carry a `broker_url` that passes [`crate::mqtt::parse_broker_url`] —
/// the credential lookup keys `creds.mqtt` by that exact URL string,
/// matching the longstanding MQTT sensor convention, so a publish broker
/// can share credentials with the sensors or carry its own.
///
/// The contract for topics, payloads, retain/QoS, and entity uniqueness is
/// ratified in `.opencode/decisions/2026-07-27-mqtt-publish-contract.md`
/// and is implemented in `dormantd::state_publisher`; this struct is the
/// declaration site only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishConfig {
    /// Publish task kill-switch. Disabled by default — the daemon does not
    /// spawn a publisher and makes no MQTT connections on the publish plane.
    #[serde(default = "default_publish_enabled")]
    pub enabled: bool,

    /// MQTT broker URL (e.g. `tcp://localhost:1883`). Required when
    /// `enabled = true`; ignored when `enabled = false`. The exact string is
    /// the credential-lookup key — operators that share a broker with their
    /// sensors reuse the same entry under `creds.mqtt`.
    #[serde(default)]
    pub broker_url: Option<String>,

    /// Base topic for state and availability publishes. Per-entity topics
    /// are formed as `{base_topic}/{instance_id}/<kind>/<id>/state`. Default
    /// `dormant`; see [`defaults::PUBLISH_BASE_TOPIC`].
    #[serde(default = "default_publish_base_topic")]
    pub base_topic: String,

    /// Home Assistant MQTT discovery prefix. Default `homeassistant`; see
    /// [`defaults::PUBLISH_DISCOVERY_PREFIX`].
    #[serde(default = "default_publish_discovery_prefix")]
    pub discovery_prefix: String,

    /// Per-instance identifier embedded in every topic. Sanitized at
    /// publish-time via the contract's `sanitize_topic_id`; the raw value
    /// is preserved here so operators can read their actual id in the web
    /// inventory and logs. Defaults to `$HOSTNAME` with a deterministic
    /// `dormant` fallback when the variable is unset or empty.
    #[serde(default = "default_publish_instance_id")]
    pub instance_id: String,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            enabled: defaults::PUBLISH_ENABLED,
            broker_url: None,
            base_topic: defaults::PUBLISH_BASE_TOPIC.into(),
            discovery_prefix: defaults::PUBLISH_DISCOVERY_PREFIX.into(),
            instance_id: default_publish_instance_id(),
        }
    }
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

    /// Optional override for the retained MQTT availability (LWT) topic.
    /// When absent, the effective availability topic is derived as
    /// `<topic>/availability` (the `Zigbee2MQTT` convention — see
    /// `dormant_sensors::mqtt::availability_topic`).
    #[serde(default)]
    pub availability_topic: Option<String>,

    /// Payload literal that marks the availability topic "online".
    #[serde(default = "default_availability_online")]
    pub availability_payload_online: String,

    /// Payload literal that marks the availability topic "offline".
    #[serde(default = "default_availability_offline")]
    pub availability_payload_offline: String,

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
    #[serde(skip_serializing_if = "Option::is_none")]
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

    /// Explicit ordering strategy (`"sequential"` or `"wear-even"`).
    /// Mutually exclusive with `shuffle`; validation rejects a config
    /// that sets both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,

    /// Luminance class applied to video items from this source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wear_tag: Option<String>,

    /// How long each image is displayed before advancing.
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
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

    /// How successive playlist items transition into each other.  One of:
    ///
    /// - `"crossfade"` (default) — a calloop-timer-driven per-pixel u8
    ///   lerp over `transition_duration` replaces the hard-cut between
    ///   playlist items.  Measured blend cost is ≈0.9 ms/frame at
    ///   3072×1728 — trivially fits any reasonable frame budget.
    /// - `"none"` — successive playlist items cut immediately (the
    ///   pre-feature behaviour; preserved for benchmarks and
    ///   operators who want the legacy look).
    ///
    /// `None` (the field absent from the TOML) is treated as `"crossfade"`.
    /// Validation rejects any other value with an `E_SCREENSAVER_SOURCE`-
    /// class error naming the allowed set.
    #[serde(default)]
    pub transition: Option<String>,

    /// Length of the crossfade blend when [`Self::transition`] is
    /// `"crossfade"`; ignored when it's `"none"`.  Bounded by the
    /// validator (100 ms ..= 10 s) — long blurs lose the visual cue that
    /// the playlist is moving; very short blurs visibly skip.
    ///
    /// `None` (the field absent from the TOML) is treated as `"1s"` at
    /// the daemon's `build_render_sinks` hop.  Read with humantime
    /// (`"500ms"`, `"1s"`).
    #[serde(default, with = "humantime_serde::option")]
    pub transition_duration: Option<Duration>,

    /// Pixel-shift distance, in pixels, applied periodically while the
    /// screensaver is active to reduce static-image burn-in risk.
    #[serde(default = "default_shift_px")]
    pub shift_px: u8,

    /// Interval between successive pixel shifts.
    #[serde(default = "default_shift_interval", with = "humantime_serde")]
    pub shift_interval: Duration,

    /// Temperature used by deterministic wear-even ordering.
    #[serde(default = "default_screensaver_wear_temperature")]
    pub wear_temperature: f64,

    /// Bias toward colder regions for screensaver pixel shifting.
    #[serde(default = "default_screensaver_shift_heat_bias")]
    pub shift_heat_bias: f64,
}

/// Whether a display is owned by this machine alone or shared between machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DisplayScope {
    /// The display is controlled only by this machine.
    #[default]
    Private,
    /// The display can be controlled by multiple coordinated machines.
    Shared,
}

/// A display definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Ordered list of controller names to try.
    pub controllers: Vec<String>,

    /// Whether this display is private to this machine or shared with peers.
    #[serde(default)]
    pub scope: DisplayScope,

    /// DDC/CI input-source value used to identify this machine's ownership of
    /// a shared display. The doctor reports the active input in hexadecimal.
    ///
    /// This is the READ code — the value the panel reports on VCP 0x60 when
    /// this input is active. Most panels use the same code for both read and
    /// write, but some (e.g. certain AOC AGON panels) accept a different value
    /// on the write path.  Use `shared_input_write_code` when write differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_input_code: Option<u8>,

    /// DDC/CI input-source value to WRITE when selecting this machine's input.
    ///
    /// Defaults to `shared_input_code`; set only when the panel accepts a
    /// different code on `setvcp 60` than it reports on `getvcp 60`.  The
    /// ownership poll always compares observations against `shared_input_code`
    /// (the read side).  The claim write path and the requester's on-wire code
    /// both use this override when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_input_write_code: Option<u8>,

    /// Peer DDC/CI input-source code this machine WRITES to switch away to the
    /// peer. Absent when push is not configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_peer_input_write_code: Option<u8>,

    /// Peer DDC/CI input-source code this machine READS to verify the peer
    /// wrote. Required when `shared_peer_input_write_code` is set for strong
    /// verification; when absent write verification degrades (see
    /// `kvm_push_verification_degraded` log event in `direct_switch.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_peer_input_code: Option<u8>,

    /// KVM hand-off actions run around release and acquire transitions.
    #[serde(default)]
    pub hooks: HookSlots,

    /// Primary blank mode to use.  Must be set unless `ladder` is provided.
    /// When `ladder` is present this field is ignored — the first
    /// `Controller(_)` stage in the ladder serves as the primary mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blank_mode: Option<BlankMode>,

    /// Fallback blank mode if the primary is unsupported (best-effort).
    /// Cannot be set when `ladder` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_mode: Option<BlankMode>,

    /// Ordered escalation ladder (replaces `blank_mode`).  Each rung is a
    /// [`LadderStage`]; the first `Controller(mode)` rung acts as the
    /// primary blank mode for the executor.
    #[serde(default)]
    pub ladder: Vec<LadderStage>,

    /// Software screensaver configuration, required when a ladder includes
    /// a [`StageKind::RenderScreensaver`] stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screensaver: Option<ScreensaverConfig>,

    /// `KWin` output name (e.g. `"DP-1"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,

    /// DDC/CI display identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddc_display: Option<String>,

    /// Hostname or IP for network-controllable displays (Samsung Tizen,
    /// LG webOS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,

    /// MAC address for Wake-on-LAN displays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wol_mac: Option<String>,

    /// Shell command to blank the display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blank_command: Option<String>,

    /// Shell command to wake the display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_command: Option<String>,

    /// Supported blank modes for `"command"` / `"ha-passthrough"` controllers
    /// (the static capability set for these controllers is empty; the user
    /// declares supported modes here).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Vec<BlankMode>>,

    /// Home Assistant URL for `"ha-passthrough"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ha_url: Option<String>,

    /// HA service to call for blanking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blank_service: Option<String>,

    /// HA service data for blanking (arbitrary TOML value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blank_data: Option<toml::Value>,

    /// HA service to call for waking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_service: Option<String>,

    /// HA service data for waking (arbitrary TOML value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_data: Option<toml::Value>,

    /// Timeout for a single blank/wake command.
    #[serde(default = "default_command_timeout", with = "humantime_serde")]
    pub command_timeout: Duration,

    /// Brightness level to restore on wake (1–100; 0 is rejected by config
    /// validation).
    #[serde(default = "default_restore_brightness")]
    pub restore_brightness: u8,

    /// Backlight to restore on wake for Samsung IP Control G2 (`backlightControl`)
    /// when the saved value is missing (daemon restart, reload, or first
    /// wake). Scale 1–50 (the TV's panel-backlight range; 0 is rejected by
    /// config validation); defaults to
    /// [`defaults::SAMSUNG_RESTORE_BACKLIGHT`] (50 — the max on that scale).
    /// Only consumed by the `samsung-tizen` controller; other controllers
    /// ignore it.
    #[serde(default = "default_samsung_restore_backlight")]
    pub samsung_restore_backlight: u8,

    /// Treat an unreachable controller as if the display is blanked
    /// (fail-safe — don't leave a screen on when we can't reach it).
    #[serde(default = "default_treat_unreachable_as_blanked")]
    pub treat_unreachable_as_blanked: bool,

    /// Panel technology classification, used to pick technology-appropriate
    /// wear heuristics (e.g. QD-OLED and W-OLED age differently under the
    /// same brightness/dwell profile).  Falls back to
    /// [`PanelType::Unknown`] when absent — a missing classification never
    /// blocks wear tracking.
    #[serde(default)]
    pub panel_type: PanelType,

    /// Explicit acknowledgement of the macOS shared-DDC/CI power-off
    /// hazard (issue #126). When a display's primary controller is DDC/CI,
    /// the panel is shared with another machine, and the primary blank
    /// mode is `power_off`, dormant emits a semantic warning at config
    /// load time and the doctor flags the topology. Setting
    /// `power_off_opt_in = true` silences the warning — the operator
    /// attests they have tested physical recovery. The opt-in adds NO
    /// recovery mechanism of its own; recovery (when the USB-C link and
    /// hub drop after a panel standby) still requires physically power-
    /// cycling the monitor. Defaults to `false` so an unsuspecting
    /// operator never lands in the trap silently.
    #[serde(default)]
    pub power_off_opt_in: bool,
}

/// The KVM hand-off hook slots available on a shared display, plus the
/// post-hoc observed-loss slot for the losing machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookSlots {
    /// Actions run before releasing a display to another machine.
    #[serde(default)]
    pub before_release: Vec<HookAction>,
    /// Actions run after releasing a display to another machine.
    #[serde(default)]
    pub after_release: Vec<HookAction>,
    /// Actions run before acquiring a display from another machine.
    #[serde(default)]
    pub before_acquire: Vec<HookAction>,
    /// Actions run after acquiring a display from another machine.
    #[serde(default)]
    pub after_acquire: Vec<HookAction>,
    /// Actions run after this machine observes (via VCP 0x60 poll) that the
    /// panel has been pulled by a peer.  Always after-the-fact — the losing
    /// machine has no advance notice.  Fire-and-forget; a hook failure here
    /// must never trigger a corrective DDC write or retry.
    #[serde(default)]
    pub on_observed_loss: Vec<HookAction>,
}

/// Command argv used by a KVM hook.
pub type HookCommand = Vec<String>;

/// MQTT publish payload used by a KVM hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMqtt {
    /// MQTT topic to publish.
    pub topic: String,
    /// Payload sent to the MQTT topic.
    pub payload: String,
}

/// One action in a KVM hand-off hook slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookAction {
    /// Command argv; mutually exclusive with [`Self::mqtt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<HookCommand>,
    /// MQTT publish; mutually exclusive with [`Self::command`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<HookMqtt>,
    /// Maximum time an action may run.
    #[serde(default = "default_hook_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// Explicit scheduling override; absent resolves from the containing slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    /// Whether a failure aborts the enclosing KVM transition.
    #[serde(default)]
    pub abort_on_failure: bool,
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

    /// True when the normalised ladder contains at least one
    /// `Controller` stage (render-only ladders have none).
    #[must_use]
    pub fn has_controller_stage(&self) -> bool {
        self.normalized_ladder()
            .iter()
            .any(|s| matches!(s.kind, StageKind::Controller(_)))
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

    /// How long to hold a display awake after a render-surface `InputWake`
    /// when every driving zone is vacant.  Zero disables the hold.
    #[serde(default = "default_input_wake_hold", with = "humantime_serde")]
    pub input_wake_hold: Duration,
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

fn default_generation_barrier_ack_timeout() -> Duration {
    defaults::GENERATION_BARRIER_ACK_TIMEOUT
}
fn default_log_level() -> String {
    defaults::LOG_LEVEL.into()
}
fn default_mqtt_field() -> String {
    defaults::MQTT_FIELD.into()
}
fn default_availability_online() -> String {
    defaults::AVAILABILITY_PAYLOAD_ONLINE.into()
}
fn default_availability_offline() -> String {
    defaults::AVAILABILITY_PAYLOAD_OFFLINE.into()
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
fn default_samsung_restore_backlight() -> u8 {
    defaults::SAMSUNG_RESTORE_BACKLIGHT
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
fn default_input_wake_hold() -> Duration {
    defaults::INPUT_WAKE_HOLD
}
fn default_web_bind() -> std::net::IpAddr {
    defaults::WEB_BIND_DEFAULT
}
fn default_entity_crud_enabled() -> bool {
    defaults::ENTITY_CRUD_ENABLED
}
fn default_pairing_enabled() -> bool {
    defaults::PAIRING_ENABLED
}

fn default_coordination_poll_interval() -> Duration {
    defaults::COORDINATION_POLL_INTERVAL
}
fn default_coordination_loss_confirmations() -> u32 {
    defaults::COORDINATION_LOSS_CONFIRMATIONS
}

fn default_activity_follow() -> bool {
    defaults::ACTIVITY_FOLLOW
}

fn default_arm_after() -> Duration {
    defaults::ARM_AFTER
}

fn default_cooldown() -> Duration {
    defaults::COOLDOWN
}

fn default_hook_timeout() -> Duration {
    defaults::HOOK_TIMEOUT
}
fn default_pair_timeout() -> Duration {
    defaults::PAIR_TIMEOUT
}
fn default_doctor_wake_settle() -> Duration {
    defaults::DOCTOR_WAKE_SETTLE
}
fn default_macos_idle_frozen_polls() -> u32 {
    defaults::MACOS_IDLE_FROZEN_POLLS
}
fn default_macos_idle_sanity_cap() -> Duration {
    defaults::MACOS_IDLE_SANITY_CAP
}
fn default_macos_idle_startup_grace() -> Duration {
    defaults::MACOS_IDLE_STARTUP_GRACE
}
fn default_trigger() -> String {
    defaults::SCREENSAVER_TRIGGER.into()
}
fn default_screensaver_audio() -> bool {
    defaults::SCREENSAVER_AUDIO
}
fn default_screensaver_wear_temperature() -> f64 {
    defaults::SCREENSAVER_WEAR_TEMPERATURE
}
fn default_screensaver_shift_heat_bias() -> f64 {
    defaults::SCREENSAVER_SHIFT_HEAT_BIAS
}
fn default_shift_px() -> u8 {
    defaults::SHIFT_PX
}
fn default_shift_interval() -> Duration {
    defaults::SHIFT_INTERVAL
}
fn default_wear_enabled() -> bool {
    defaults::WEAR_ENABLED
}
fn default_wear_sample_interval() -> Duration {
    defaults::WEAR_SAMPLE_INTERVAL
}
fn default_wear_persist_interval() -> Duration {
    defaults::WEAR_PERSIST_INTERVAL
}
fn default_wear_read_timeout() -> Duration {
    defaults::WEAR_READ_TIMEOUT
}
fn default_wear_grid_rows() -> u16 {
    defaults::WEAR_GRID_ROWS
}
fn default_wear_grid_cols() -> u16 {
    defaults::WEAR_GRID_COLS
}
fn default_wear_fallback_brightness() -> f64 {
    defaults::WEAR_FALLBACK_BRIGHTNESS
}
fn default_wear_screensaver_factor() -> f64 {
    defaults::WEAR_SCREENSAVER_FACTOR
}
fn default_wear_short_cycle_dwell() -> Duration {
    defaults::WEAR_SHORT_CYCLE_DWELL
}
fn default_wear_advisory_after() -> Duration {
    defaults::WEAR_ADVISORY_AFTER
}
fn default_active_sampling_enabled() -> bool {
    defaults::WEAR_ACTIVE_SAMPLING_ENABLED
}
fn default_active_sampling_stream_mode() -> StreamMode {
    match defaults::WEAR_ACTIVE_SAMPLING_STREAM_MODE {
        "per-tick" => StreamMode::PerTick,
        _ => StreamMode::Warm,
    }
}
fn default_active_sampling_capture_timeout() -> Duration {
    defaults::WEAR_ACTIVE_SAMPLING_CAPTURE_TIMEOUT
}
fn default_active_sampling_failure_threshold() -> u32 {
    defaults::WEAR_ACTIVE_SAMPLING_FAILURE_THRESHOLD
}
fn default_active_sampling_circuit_reset_after() -> Duration {
    defaults::WEAR_ACTIVE_SAMPLING_CIRCUIT_RESET_AFTER
}
fn default_notify_enabled() -> bool {
    defaults::NOTIFY_ENABLED
}
fn default_notify_wake_attempt_threshold() -> u64 {
    defaults::NOTIFY_WAKE_ATTEMPT_THRESHOLD
}
fn default_notify_cooldown() -> Duration {
    defaults::NOTIFY_COOLDOWN
}
fn default_notify_recovery() -> bool {
    defaults::NOTIFY_RECOVERY
}
fn default_lkg_enabled() -> bool {
    defaults::LKG_ENABLED
}
fn default_lkg_rollback_enabled() -> bool {
    defaults::LKG_ROLLBACK_ENABLED
}
fn default_lkg_stability_window() -> Duration {
    defaults::LKG_STABILITY_WINDOW
}
fn default_audio_poll_interval() -> Duration {
    defaults::AUDIO_POLL_INTERVAL
}
fn default_audio_min_active() -> Duration {
    defaults::AUDIO_MIN_ACTIVE
}
fn default_audio_call_roles() -> Vec<String> {
    defaults::AUDIO_CALL_ROLES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}
fn default_audio_pw_dump_command() -> String {
    defaults::AUDIO_PW_DUMP_COMMAND.into()
}
fn default_publish_enabled() -> bool {
    defaults::PUBLISH_ENABLED
}
fn default_publish_base_topic() -> String {
    defaults::PUBLISH_BASE_TOPIC.into()
}
fn default_publish_discovery_prefix() -> String {
    defaults::PUBLISH_DISCOVERY_PREFIX.into()
}

/// Std-only hostname lookup for `publish.instance_id`. Reads `$HOSTNAME`
/// (set on every mainstream Linux/BSD/macOS shell) and falls back to the
/// deterministic literal `dormant` when unset or empty. Container
/// environments sometimes omit the variable; the fallback keeps the topic
/// tree stable across daemon restarts on the same host.
fn default_publish_instance_id() -> String {
    let raw = std::env::var("HOSTNAME").unwrap_or_default();
    if raw.is_empty() {
        defaults::PUBLISH_INSTANCE_ID_FALLBACK.into()
    } else {
        raw
    }
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

    // ── AudioConfig ──────────────────────────────────────────────────────

    #[test]
    fn audio_config_defaults_when_section_absent() {
        let cfg: Config = toml::from_str("config_version = 1\n").unwrap();
        assert_eq!(cfg.audio, AudioConfig::default());
        assert_eq!(cfg.audio.poll_interval, defaults::AUDIO_POLL_INTERVAL);
        assert_eq!(cfg.audio.min_active, defaults::AUDIO_MIN_ACTIVE);
        assert_eq!(cfg.audio.call_roles, vec!["Communication".to_string()]);
        assert_eq!(cfg.audio.playback_roles, None);
        assert!(!cfg.audio.capture_is_call);
        assert_eq!(cfg.audio.pw_dump_command, "pw-dump");
    }

    #[test]
    fn audio_config_defaults_when_section_empty() {
        let cfg: Config = toml::from_str("config_version = 1\n[audio]\n").unwrap();
        assert_eq!(cfg.audio, AudioConfig::default());
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

    // ── [wear] section ────────────────────────────────────────────────────

    #[test]
    fn wear_defaults_load_from_empty_section() {
        let toml_str = "config_version = 1\n[wear]\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.wear.enabled);
        assert_eq!(cfg.wear.grid_rows, 9);
        assert_eq!(cfg.wear.grid_cols, 16);
        assert_eq!(cfg.wear.sample_interval, Duration::from_secs(60));
        assert_eq!(cfg.wear.persist_interval, Duration::from_secs(5 * 60));
        assert_eq!(cfg.wear.read_timeout, Duration::from_secs(2));
        assert!((cfg.wear.fallback_brightness - 0.5).abs() < f64::EPSILON);
        assert!((cfg.wear.screensaver_factor - 0.35).abs() < f64::EPSILON);
        assert_eq!(cfg.wear.short_cycle_dwell, Duration::from_secs(10 * 60));
        assert_eq!(cfg.wear.advisory_after, Duration::from_secs(96 * 3600));
    }

    #[test]
    fn wear_defaults_when_section_absent() {
        let toml_str = "config_version = 1\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.wear.enabled);
        assert_eq!(cfg.wear.grid_rows, 9);
        assert_eq!(cfg.wear.sample_interval, Duration::from_secs(60));
    }

    #[test]
    fn wear_explicit_values_parse() {
        let toml_str = r#"
config_version = 1

[wear]
enabled = false
sample_interval = "30s"
persist_interval = "10m"
read_timeout = "1s"
grid_rows = 12
grid_cols = 20
fallback_brightness = 0.7
screensaver_factor = 0.2
short_cycle_dwell = "5m"
advisory_after = "48h"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.wear.enabled);
        assert_eq!(cfg.wear.sample_interval, Duration::from_secs(30));
        assert_eq!(cfg.wear.persist_interval, Duration::from_secs(600));
        assert_eq!(cfg.wear.read_timeout, Duration::from_secs(1));
        assert_eq!(cfg.wear.grid_rows, 12);
        assert_eq!(cfg.wear.grid_cols, 20);
        assert!((cfg.wear.fallback_brightness - 0.7).abs() < f64::EPSILON);
        assert!((cfg.wear.screensaver_factor - 0.2).abs() < f64::EPSILON);
        assert_eq!(cfg.wear.short_cycle_dwell, Duration::from_secs(300));
        assert_eq!(cfg.wear.advisory_after, Duration::from_secs(48 * 3600));
    }

    #[test]
    fn active_sampling_defaults_when_section_absent_or_empty() {
        for toml_str in [
            "config_version = 1\n",
            "config_version = 1\n[wear.active_sampling]\n",
        ] {
            let cfg: Config = toml::from_str(toml_str).unwrap();
            assert!(!cfg.wear.active_sampling.enabled);
            assert_eq!(cfg.wear.active_sampling.sampled_display, None);
            assert_eq!(cfg.wear.active_sampling.stream_mode, StreamMode::Warm);
            assert_eq!(
                cfg.wear.active_sampling.capture_timeout,
                Duration::from_secs(2)
            );
            assert_eq!(cfg.wear.active_sampling.failure_threshold, 5);
            assert_eq!(
                cfg.wear.active_sampling.circuit_reset_after,
                Duration::from_secs(300)
            );
        }
    }

    #[test]
    fn active_sampling_explicit_values_and_stream_modes_parse() {
        for mode in ["warm", "per-tick"] {
            let toml_str = format!(
                "config_version = 1\n[wear.active_sampling]\nenabled = true\nsampled_display = \"desk\"\nstream_mode = \"{mode}\"\ncapture_timeout = \"3s\"\nfailure_threshold = 7\ncircuit_reset_after = \"1m\"\n"
            );
            let cfg: Config = toml::from_str(&toml_str).unwrap();
            assert!(cfg.wear.active_sampling.enabled);
            assert_eq!(
                cfg.wear.active_sampling.sampled_display.as_deref(),
                Some("desk")
            );
            assert_eq!(
                cfg.wear.active_sampling.selected_displays(),
                vec!["desk".to_owned()]
            );
            assert_eq!(
                cfg.wear.active_sampling.capture_timeout,
                Duration::from_secs(3)
            );
            assert_eq!(cfg.wear.active_sampling.failure_threshold, 7);
            assert_eq!(
                cfg.wear.active_sampling.circuit_reset_after,
                Duration::from_secs(60)
            );
            assert_eq!(
                cfg.wear.active_sampling.stream_mode,
                if mode == "warm" {
                    StreamMode::Warm
                } else {
                    StreamMode::PerTick
                }
            );
        }
    }

    #[test]
    fn active_sampling_accepts_legacy_singular_sampled_display() {
        // Backward compatibility: a config that supplies only the legacy
        // singular `sampled_display` is treated as one selected display.
        let toml_str = r#"
config_version = 1
[displays.desk]
controllers = ["kwin-dpms"]
[wear.active_sampling]
enabled = true
sampled_display = "desk"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.wear.active_sampling.sampled_display.as_deref(),
            Some("desk")
        );
        assert_eq!(
            cfg.wear.active_sampling.selected_displays(),
            vec!["desk".to_owned()]
        );
    }

    #[test]
    fn active_sampling_plural_sampled_displays_parses_two() {
        let toml_str = r#"
config_version = 1
[displays.desk]
controllers = ["kwin-dpms"]
[displays.tv]
controllers = ["samsung-tizen"]
[wear.active_sampling]
enabled = true
sampled_displays = ["desk", "tv"]
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.wear.active_sampling.selected_displays(),
            vec!["desk".to_owned(), "tv".to_owned()]
        );
    }

    #[test]
    fn active_sampling_rejects_both_singular_and_plural_keys() {
        // Supplying both keys is rejected explicitly so the operator's
        // intent is unambiguous. Silent precedence between the two
        // forms would make config review misrepresent which displays
        // are captured.
        let toml_str = r#"
config_version = 1
[displays.desk]
controllers = ["kwin-dpms"]
[wear.active_sampling]
enabled = true
sampled_display = "desk"
sampled_displays = ["desk"]
"#;
        let error = toml::from_str::<Config>(toml_str).unwrap_err().to_string();
        assert!(
            error.contains("sampled_display") && error.contains("sampled_displays"),
            "expected error to mention both keys, got: {error}"
        );
    }

    #[test]
    fn active_sampling_rejects_duplicate_ids_in_plural_list() {
        let toml_str = r#"
config_version = 1
[displays.desk]
controllers = ["kwin-dpms"]
[wear.active_sampling]
enabled = true
sampled_displays = ["desk", "desk"]
"#;
        let error = toml::from_str::<Config>(toml_str).unwrap_err().to_string();
        assert!(
            error.to_lowercase().contains("duplicate"),
            "expected duplicate-key error, got: {error}"
        );
    }

    #[test]
    fn active_sampling_rejects_sanitized_collision_ids() {
        // "desk!" and "desk?" both sanitize to "desk-" (the `!` and
        // `?` collapse to `-`), so they would race for the same
        // on-disk consent record. Reject at config parse so the
        // operator is told up front rather than discovering it at
        // sampler spawn.
        let toml_str = r#"
config_version = 1
[displays."desk!"]
controllers = ["kwin-dpms"]
[displays."desk?"]
controllers = ["kwin-dpms"]
[wear.active_sampling]
enabled = true
sampled_displays = ["desk!", "desk?"]
"#;
        let error = toml::from_str::<Config>(toml_str).unwrap_err().to_string();
        assert!(
            error.to_lowercase().contains("collision") || error.to_lowercase().contains("sanitiz"),
            "expected sanitized-collision error, got: {error}"
        );
    }

    #[test]
    fn active_sampling_unknown_key_passes_parse_for_walker() {
        // The repo's two-stage discipline: an unknown key under
        // [wear.active_sampling] must PARSE successfully so the
        // `validate::collect_unknown_keys` walker can surface it with
        // `E_CONFIG_UNKNOWN_KEY`. A hand-rolled deserializer that hard-
        // errors on unknown keys bypasses the walker and changes the
        // error stage; the dispatch's parity test pins the soft path.
        let toml_str = r#"
config_version = 1
[wear.active_sampling]
sampled_display = "desk"
bogus_key = "should be flagged by the walker, not rejected at parse"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.wear.active_sampling.sampled_display.as_deref(),
            Some("desk"),
            "known keys must still bind cleanly when an unknown is present",
        );
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = super::super::validate::collect_unknown_keys(&value);
        assert!(
            unknown
                .iter()
                .any(|u| u.key_path == "wear.active_sampling.bogus_key"),
            "walker must surface the unknown key, got {:?}",
            unknown.iter().map(|u| &u.key_path).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn first_sampled_display_prefers_singular_then_one_element_plural() {
        // The runtime's single-display path reads `first_sampled_display`
        // so a 1-element `sampled_displays` list drives the existing
        // consent flow without waiting for the multi-display registry.
        // 0- and >1-element plural lists return `None` (suspended).
        let singular = ActiveSamplingConfig {
            enabled: true,
            sampled_display: Some("desk".to_owned()),
            sampled_displays: Vec::new(),
            stream_mode: StreamMode::Warm,
            capture_timeout: Duration::from_secs(2),
            failure_threshold: 5,
            circuit_reset_after: Duration::from_secs(300),
        };
        assert_eq!(singular.first_sampled_display(), Some("desk"));

        let one_element = ActiveSamplingConfig {
            enabled: true,
            sampled_display: None,
            sampled_displays: vec!["tv".to_owned()],
            stream_mode: StreamMode::Warm,
            capture_timeout: Duration::from_secs(2),
            failure_threshold: 5,
            circuit_reset_after: Duration::from_secs(300),
        };
        assert_eq!(one_element.first_sampled_display(), Some("tv"));

        let two_elements = ActiveSamplingConfig {
            enabled: true,
            sampled_display: None,
            sampled_displays: vec!["desk".to_owned(), "tv".to_owned()],
            stream_mode: StreamMode::Warm,
            capture_timeout: Duration::from_secs(2),
            failure_threshold: 5,
            circuit_reset_after: Duration::from_secs(300),
        };
        assert_eq!(two_elements.first_sampled_display(), None);

        let none = ActiveSamplingConfig::default();
        assert_eq!(none.first_sampled_display(), None);
    }

    // ── DisplayConfig::panel_type ───────────────────────────────────────────

    #[test]
    fn panel_type_parses_and_defaults() {
        let toml_str = r#"
config_version = 1

[displays.m]
controllers = ["command"]
blank_command = "true"
wake_command = "true"
modes = ["power_off"]
panel_type = "qd-oled"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.displays["m"].panel_type, PanelType::QdOled);
    }

    #[test]
    fn panel_type_defaults_to_unknown_when_absent() {
        let toml_str = r#"
config_version = 1

[displays.m]
controllers = ["command"]
blank_command = "true"
wake_command = "true"
modes = ["power_off"]
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.displays["m"].panel_type, PanelType::Unknown);
    }

    // ── ScreensaverConfig shift_px / shift_interval ─────────────────────────

    #[test]
    fn shift_keys_parse_explicit_values() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
shift_px = 4
shift_interval = "90s"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let ss = cfg.displays["d1"].screensaver.as_ref().unwrap();
        assert_eq!(ss.shift_px, 4);
        assert_eq!(ss.shift_interval, Duration::from_secs(90));
    }

    #[test]
    fn shift_keys_default_when_absent() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let ss = cfg.displays["d1"].screensaver.as_ref().unwrap();
        assert_eq!(ss.shift_px, 2);
        assert_eq!(ss.shift_interval, Duration::from_secs(120));
    }

    // ── [notifications] section ─────────────────────────────────────────

    #[test]
    fn notifications_defaults_load_from_empty_and_absent_section() {
        let cfg: Config = toml::from_str("config_version = 1\n").unwrap();
        assert!(cfg.notifications.enabled);
        assert_eq!(cfg.notifications.wake_attempt_threshold, 3);
        assert_eq!(cfg.notifications.cooldown, Duration::from_secs(15 * 60));
        assert!(cfg.notifications.notify_recovery);

        let cfg: Config = toml::from_str("config_version = 1\n[notifications]\n").unwrap();
        assert!(cfg.notifications.enabled);
        assert_eq!(cfg.notifications.wake_attempt_threshold, 3);
        assert_eq!(cfg.notifications.cooldown, Duration::from_secs(15 * 60));
        assert!(cfg.notifications.notify_recovery);
    }

    #[test]
    fn notifications_explicit_values_parse() {
        let toml_str = r#"
config_version = 1

[notifications]
enabled = false
wake_attempt_threshold = 5
cooldown = "30m"
notify_recovery = false
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.notifications.enabled);
        assert_eq!(cfg.notifications.wake_attempt_threshold, 5);
        assert_eq!(cfg.notifications.cooldown, Duration::from_secs(30 * 60));
        assert!(!cfg.notifications.notify_recovery);
    }

    // ── MqttSensorCfg availability keys ─────────────────────────────────────

    #[test]
    fn availability_keys_default_and_load() {
        let toml_str = r#"
config_version = 1
[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "zigbee2mqtt/desk"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let SensorConfig::Mqtt(m) = &cfg.sensors["desk"] else {
            panic!("expected Mqtt sensor")
        };
        assert_eq!(m.availability_topic, None);
        assert_eq!(m.availability_payload_online, "online");
        assert_eq!(m.availability_payload_offline, "offline");
    }

    #[test]
    fn availability_keys_explicit_values_parse() {
        let toml_str = r#"
config_version = 1
[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "zigbee2mqtt/desk"
availability_topic = "zigbee2mqtt/desk/avail"
availability_payload_online = "up"
availability_payload_offline = "down"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let SensorConfig::Mqtt(m) = &cfg.sensors["desk"] else {
            panic!("expected Mqtt sensor")
        };
        assert_eq!(
            m.availability_topic.as_deref(),
            Some("zigbee2mqtt/desk/avail")
        );
        assert_eq!(m.availability_payload_online, "up");
        assert_eq!(m.availability_payload_offline, "down");
    }

    // ── Task 5: macOS idle config additive surface ──────────────────────────

    #[test]
    fn macos_idle_config_is_additive_and_has_documented_defaults() {
        let toml_str = r#"
config_version = 1
[daemon]
idle_source = "macos"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.daemon.idle_source, IdleSource::Macos);
        assert_eq!(cfg.daemon.macos_idle_frozen_polls, 3);
        assert_eq!(
            cfg.daemon.macos_idle_sanity_cap,
            std::time::Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(
            cfg.daemon.macos_idle_startup_grace,
            std::time::Duration::from_secs(15)
        );
    }

    #[test]
    fn macos_idle_config_defaults_when_daemon_absent() {
        let cfg: Config = toml::from_str("config_version = 1\n").unwrap();
        assert_eq!(cfg.daemon.idle_source, IdleSource::Auto);
        assert_eq!(cfg.daemon.macos_idle_frozen_polls, 3);
        assert_eq!(
            cfg.daemon.macos_idle_sanity_cap,
            std::time::Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(
            cfg.daemon.macos_idle_startup_grace,
            std::time::Duration::from_secs(15)
        );
    }

    #[test]
    fn display_scope_defaults_private_and_omits_shared_code() {
        let cfg: Config = toml::from_str(
            "config_version = 1\n[displays.desk]\ncontrollers = [\"ddcci\"]\nblank_mode = \"power_off\"\n",
        )
        .unwrap();

        assert_eq!(cfg.displays["desk"].scope, DisplayScope::Private);
        assert_eq!(cfg.displays["desk"].shared_input_code, None);
    }

    #[test]
    fn shared_display_keys_parse_in_strict_mode() {
        let cfg: Config = toml::from_str(
            "config_version = 1\n[displays.desk]\ncontrollers = [\"ddcci\"]\nblank_mode = \"power_off\"\nscope = \"shared\"\nshared_input_code = 15\n",
        )
        .unwrap();

        assert_eq!(cfg.displays["desk"].scope, DisplayScope::Shared);
        assert_eq!(cfg.displays["desk"].shared_input_code, Some(15));
    }

    #[test]
    fn coordination_defaults_are_opt_in_with_activity_follow() {
        let cfg: Config = toml::from_str("config_version = 1\n").unwrap();

        assert!(!cfg.coordination.activity_follow);
        assert_eq!(cfg.coordination.poll_interval, Duration::from_secs(2));
        assert_eq!(cfg.coordination.state_poll_interval, None);
        // Absent key resolves to max(30s, poll_interval=2s) = 30s.
        assert_eq!(
            cfg.coordination.effective_state_poll_interval(),
            Duration::from_secs(30)
        );
        // loss_confirmations default — defends against issue #134 garbled reads.
        assert_eq!(cfg.coordination.loss_confirmations, 3);
        assert_eq!(cfg.coordination.arm_after, Duration::from_secs(7));
        assert_eq!(cfg.coordination.cooldown, Duration::from_secs(3));
    }

    #[test]
    fn coordination_parses_surviving_fields() {
        let cfg: Config = toml::from_str(
            "config_version = 1\n[coordination]\npoll_interval = \"3s\"\nstate_poll_interval = \"30s\"\nactivity_follow = true\narm_after = \"5s\"\ncooldown = \"10s\"\n",
        )
        .unwrap();

        assert!(cfg.coordination.activity_follow);
        assert_eq!(cfg.coordination.poll_interval, Duration::from_secs(3));
        assert_eq!(
            cfg.coordination.state_poll_interval,
            Some(Duration::from_secs(30))
        );
        assert_eq!(cfg.coordination.arm_after, Duration::from_secs(5));
        assert_eq!(cfg.coordination.cooldown, Duration::from_secs(10));
    }

    #[test]
    fn coordination_state_poll_interval_absent_resolves_to_max_of_floor_and_poll() {
        // Absent key: effective cadence is max(30s, poll_interval). A fixed 30s
        // default would sit below poll_interval=60s and fail validation.
        let cfg: Config =
            toml::from_str("config_version = 1\n[coordination]\npoll_interval = \"60s\"\n")
                .unwrap();
        assert_eq!(cfg.coordination.state_poll_interval, None);
        assert_eq!(
            cfg.coordination.effective_state_poll_interval(),
            Duration::from_secs(60)
        );

        // Default poll_interval (2s) → max(30s, 2s) = 30s.
        let cfg_default: Config = toml::from_str("config_version = 1\n").unwrap();
        assert_eq!(cfg_default.coordination.state_poll_interval, None);
        assert_eq!(
            cfg_default.coordination.effective_state_poll_interval(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn coordination_state_poll_interval_explicit_value_round_trips() {
        let cfg: Config = toml::from_str(
            "config_version = 1\n[coordination]\npoll_interval = \"2s\"\nstate_poll_interval = \"10s\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.coordination.state_poll_interval,
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            cfg.coordination.effective_state_poll_interval(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn kvm_sections_parse_additively_in_strict_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kvm_full.toml");
        std::fs::write(&path, include_str!("../../tests/fixtures/kvm_full.toml")).unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        assert!(cfg.coordination.activity_follow);
        assert_eq!(cfg.keymap.claim_hotkey.as_deref(), Some("Meta+F12"));
        assert_eq!(cfg.input_filter.ignore_devices, vec!["*jiggler*"]);
        assert_eq!(cfg.displays["monitor"].hooks.before_release.len(), 1);
    }

    #[test]
    fn strict_mode_rejects_unknown_nested_hook_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kvm_typo.toml");
        std::fs::write(
            &path,
            format!(
                "{}\nbefore_releasee = []\n",
                include_str!("../../tests/fixtures/kvm_full.toml")
            ),
        )
        .unwrap();
        assert!(crate::config::load_config(&path, Strictness::Strict).is_err());
    }
}
