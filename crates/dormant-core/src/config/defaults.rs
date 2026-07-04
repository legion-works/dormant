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

/// Brightness to restore when waking a display (0–100).
/// 80 is a sane daytime default that doesn't require per-display tuning.
pub const RESTORE_BRIGHTNESS: u8 = 80;

/// Default log level for the daemon.
pub const LOG_LEVEL: &str = "info";

/// Default baud rate for USB-connected LD2410 mmWave radar modules.
pub const LD2410_BAUD: u32 = 256_000;

/// Default JSON-pointer field read from MQTT payloads.
pub const MQTT_FIELD: &str = "/occupancy";
