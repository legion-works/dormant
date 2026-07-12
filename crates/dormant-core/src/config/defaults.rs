//! Default configuration values — the single grep-stable source for every
//! tunable default in dormant.  Every const is a `pub const` with a doc comment
//! so that `dormantctl config-schema` and `rustdoc` both pick them up.
//!
//! [`schema`](super::schema) references these via `#[serde(default = "...")]`
//! function shims so that serde fills in the default when a key is absent.

use std::time::Duration;

/// How long the daemon waits after startup before beginning any blank/wake
/// actions (allows sensors to stabilise).
pub const STARTUP_HOLDOFF: Duration = Duration::from_secs(30);

/// How long a sensor can go without producing an event before it is considered
/// stale.  A stale sensor triggers the zone's [`UnavailablePolicy`].
///
/// [`UnavailablePolicy`]: crate::zone::UnavailablePolicy
pub const STALE_SENSOR_TIMEOUT: Duration = Duration::from_secs(300);

/// Debounce window that coalesces rapid config-file changes into a single
/// reload (editors often write-then-rename, producing several events).
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// How long a zone must stay present or absent before a rule acts (debounce).
pub const GRACE_PERIOD: Duration = Duration::from_secs(60);

/// Minimum time a display must stay blanked before it can be woken again.
pub const MIN_BLANK_TIME: Duration = Duration::from_secs(10);

/// Minimum time a display must stay awake before it can be blanked again.
pub const MIN_WAKE_TIME: Duration = Duration::from_secs(10);

/// Idle threshold for user-activity inhibitors — no keyboard/mouse events for
/// this long means the user is considered inactive.
pub const ACTIVITY_IDLE_THRESHOLD: Duration = Duration::from_secs(120);

/// How often to poll user-activity state while an activity inhibitor is active.
pub const ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Number of wake retries before escalating to the next controller or failing.
pub const WAKE_RETRIES: u32 = 3;

/// Backoff between the immediate wake attempt and the first retry.
pub const WAKE_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Interval between successive wake retries after the initial backoff.
pub const WAKE_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Timeout for a single blank/wake command before the controller considers it
/// failed and moves to the next retry or escalation step.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Brightness to restore when waking a display (1–100; 0 is rejected by
/// config validation to guarantee a lit panel on wake).
/// 80 is a sane daytime default that doesn't require per-display tuning.
pub const RESTORE_BRIGHTNESS: u8 = 80;

/// Backlight to restore on wake for Samsung IP Control G2 (`backlightControl`)
/// when the saved value is missing (daemon restart, reload, or first wake).
/// Scale 1–50 (the TV's panel-backlight range; 0 is rejected by config
/// validation). 50 is the max — the fail-safe-toward-screens-on doctrine
/// accepts a too-bright panel; a stuck-dim one is not acceptable.
pub const SAMSUNG_RESTORE_BACKLIGHT: u8 = 50;

/// Which idle source to use for the activity inhibitor (`"auto"` | `"wayland"` |
/// `"dbus"`). `"auto"` prefers Wayland when available, falling back to `DBus`.
pub const IDLE_SOURCE: &str = "auto";

/// Default log level for the daemon.
pub const LOG_LEVEL: &str = "info";

/// Default baud rate for USB-connected LD2410 mmWave radar modules.
pub const LD2410_BAUD: u32 = 256_000;

/// Default JSON-pointer field read from MQTT payloads.
pub const MQTT_FIELD: &str = "/occupancy";

/// Default payload literal that marks an MQTT availability (LWT) topic
/// "online" — the `Zigbee2MQTT`/Home-Assistant-MQTT convention.
pub const AVAILABILITY_PAYLOAD_ONLINE: &str = "online";

/// Default payload literal that marks an MQTT availability (LWT) topic
/// "offline" — the `Zigbee2MQTT`/Home-Assistant-MQTT convention.
pub const AVAILABILITY_PAYLOAD_OFFLINE: &str = "offline";

/// Default web-UI bind address — loopback only (operator tool).
pub const WEB_BIND_DEFAULT: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

/// Whether the web UI's entity create/delete affordances are enabled by
/// default.
pub const ENTITY_CRUD_ENABLED: bool = true;

/// Whether the Samsung pairing wizard route is enabled by default.
pub const PAIRING_ENABLED: bool = true;

/// Default timeout for a single pairing-wizard attempt (validated to
/// `30s..=300s` — see [`mod@super::validate`]).
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(120);

/// Default duration each screensaver source image is displayed (8 seconds).
pub const IMAGE_DURATION: Duration = Duration::from_secs(8);

/// Whether the screensaver overlay should play audio by default.
pub const SCREENSAVER_AUDIO: bool = false;

/// Default trigger for the screensaver overlay.
pub const SCREENSAVER_TRIGGER: &str = "vacancy";

/// Default mpv cache size for screensaver playback (64 MiB).
/// Kept modest to avoid memory pressure on embedded / low-RAM hosts.
pub const MPV_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Default crossfade duration when [`super::schema::ScreensaverConfig::transition`]
/// is `"crossfade"`.  One second reads as a deliberate transition without
/// dragging the playlist — measured crossfade cost is ≈0.9 ms/frame at
/// 3072×1728, so longer blends are essentially free at any sane display
/// resolution.
pub const TRANSITION_DURATION: Duration = Duration::from_secs(1);

/// `#[serde(default = "default_trigger")]` function shim — returns the default
/// trigger string for [`super::schema::ScreensaverConfig::trigger`].
#[must_use]
pub fn default_trigger() -> String {
    SCREENSAVER_TRIGGER.to_string()
}

// ── [wear] section defaults ─────────────────────────────────────────────────

/// Whether panel-wear tracking is enabled by default.
pub const WEAR_ENABLED: bool = true;

/// How often the wear tracker samples panel state for attribution.
pub const WEAR_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// How often the wear tracker persists its ledger to disk.
pub const WEAR_PERSIST_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Timeout for a single panel-state read during wear sampling.
pub const WEAR_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Default number of rows in the wear-attribution grid.
pub const WEAR_GRID_ROWS: u16 = 9;

/// Default number of columns in the wear-attribution grid.
pub const WEAR_GRID_COLS: u16 = 16;

/// Default brightness fraction (0.0–1.0) assumed when the real brightness
/// can't be read from the panel.
pub const WEAR_FALLBACK_BRIGHTNESS: f64 = 0.5;

/// Default brightness fraction (0.0–1.0) attributed while the screensaver is
/// active.
pub const WEAR_SCREENSAVER_FACTOR: f64 = 0.35;

/// Minimum dwell before a blank/wake cycle counts as a "full" cycle rather
/// than a short cycle for wear-cycle-count heuristics.
pub const WEAR_SHORT_CYCLE_DWELL: Duration = Duration::from_secs(10 * 60);

/// Panel age (accumulated on-hours) after which wear advisories start
/// surfacing to the operator.
pub const WEAR_ADVISORY_AFTER: Duration = Duration::from_secs(96 * 60 * 60);

// ── [notifications] section defaults ────────────────────────────────────────

/// Whether wake-failure notifications are enabled by default.
pub const NOTIFY_ENABLED: bool = true;

/// Number of consecutive wake-command failures before a notification fires.
pub const NOTIFY_WAKE_ATTEMPT_THRESHOLD: u64 = 3;

/// Minimum time between successive wake-failure notifications for the same
/// display.
pub const NOTIFY_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// Whether a recovery notification fires when a previously-failing display
/// wakes successfully again.
pub const NOTIFY_RECOVERY: bool = true;

// ── [watchdog] section defaults ─────────────────────────────────────────────

/// Whether last-known-good (LKG) config-generation tracking is enabled by
/// default.
pub const LKG_ENABLED: bool = true;

/// Whether a detected crash loop is allowed to trigger an automatic rollback
/// to the last-known-good generation by default.
pub const LKG_ROLLBACK_ENABLED: bool = true;

/// Default minimum uptime before a boot counts as stable for LKG purposes.
pub const LKG_STABILITY_WINDOW: Duration = Duration::from_secs(5 * 60);

// ── Screensaver pixel-shift defaults ────────────────────────────────────────

/// Default pixel-shift distance, in pixels, applied periodically while the
/// screensaver is active to reduce static-image burn-in risk.
pub const SHIFT_PX: u8 = 2;

/// Default interval between successive pixel shifts.
pub const SHIFT_INTERVAL: Duration = Duration::from_secs(120);

// ── [audio] section defaults ────────────────────────────────────────────────

/// How often the audio inhibitor polls `pw_dump_command` for the current
/// `PipeWire` graph state.
pub const AUDIO_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum continuous stream activity before the audio inhibitor asserts
/// inhibition (debounces transient blips; bypassed once at poller startup
/// for a stream already running when the daemon starts — see the audio
/// source's `startup_grace`).
pub const AUDIO_MIN_ACTIVE: Duration = Duration::from_secs(3);

/// `media.role` values that mean "this running stream is a call".
pub const AUDIO_CALL_ROLES: &[&str] = &["Communication"];

/// Default `pw-dump` invocation (resolved via `$PATH`); override via
/// `[audio].pw_dump_command` — the test/override seam.
pub const AUDIO_PW_DUMP_COMMAND: &str = "pw-dump";
