//! Domain types for dormant: sensor/display/zone identifiers, sensor state,
//! presence events, timestamps, blank modes, and command-failure records.
//!
//! All public types are `#[derive]`-heavy and serde-compatible where appropriate.
//! Newtype ids use `#[serde(transparent)]` so they serialize as plain strings.

use std::fmt;
use std::time::Duration;

// ── Newtype IDs ───────────────────────────────────────────────────────────────

/// Identifier for a presence sensor.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SensorId(pub String);

impl fmt::Display for SensorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for a display.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct DisplayId(pub String);

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for a zone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ZoneId(pub String);

impl fmt::Display for ZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for a rule.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RuleId(pub String);

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Sensor state ──────────────────────────────────────────────────────────────

/// Whether a sensor reports a person present, absent, or unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorState {
    /// Someone is detected.
    Present,
    /// No one is detected.
    Absent,
    /// The sensor has no data (offline, unplugged, broker down).
    Unavailable,
}

// ── Timestamp (wall clock) ────────────────────────────────────────────────────

/// A wall-clock timestamp, wrapping [`std::time::SystemTime`].
///
/// Use [`Timestamp::now`] to capture the current time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Timestamp(pub std::time::SystemTime);

impl Timestamp {
    /// Create a `Timestamp` representing the current system time.
    #[must_use]
    pub fn now() -> Self {
        Self(std::time::SystemTime::now())
    }
}

// ── Tick (monotonic) ──────────────────────────────────────────────────────────

/// A monotonic tick, wrapping [`std::time::Instant`].
///
/// Use [`Tick::now`] to capture the current instant.  Not serializable because
/// `Instant` is opaque and platform-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tick(pub std::time::Instant);

impl Tick {
    /// Create a `Tick` representing the current monotonic instant.
    ///
    /// Under the hood this delegates to [`tokio::time::Instant::now`] so that
    /// paused-time tests (`#[tokio::test(start_paused = true)]`) see the
    /// virtual clock — without that delegation the engine's grace countdown
    /// and stale-sensor sweeper would march against wall-clock time and the
    /// tests could not advance minutes in milliseconds.
    #[must_use]
    pub fn now() -> Self {
        Self(tokio::time::Instant::now().into_std())
    }
}

// ── Presence event ────────────────────────────────────────────────────────────

/// A sensor-reported presence observation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PresenceEvent {
    /// The sensor that produced this event.
    pub sensor_id: SensorId,
    /// The observed state.
    pub state: SensorState,
    /// Confidence in the observation (0.0 – 1.0).
    pub confidence: f32,
    /// When the observation was made.
    pub at: Timestamp,
}

impl PresenceEvent {
    /// Create a new presence event with full confidence (1.0).
    #[must_use]
    pub fn new(sensor_id: SensorId, state: SensorState, at: Timestamp) -> Self {
        Self {
            sensor_id,
            state,
            confidence: 1.0,
            at,
        }
    }
}

// ── Blank mode ────────────────────────────────────────────────────────────────

/// How a display should be blanked when the room is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlankMode {
    /// Send a power-off signal (DPMS off, DDC power off, etc.).
    PowerOff,
    /// Turn the screen off but keep audio active.
    ScreenOffAudioOn,
    /// Set brightness to zero (backlight off, display still on).
    BrightnessZero,
}

// ── Stage kind ───────────────────────────────────────────────────────────────

/// A single step in the escalation ladder — either a hardware controller blank
/// mode or a fallback software render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    /// A display-controller blank mode (DPMS, DDC/CI, CEC, etc.).
    Controller(BlankMode),
    /// Render a black full-screen overlay (software blank).
    RenderBlack,
    /// Render a screensaver (last-resort fallback).
    RenderScreensaver,
}

impl StageKind {
    /// True for the two render variants — these run as software overlays
    /// and should only be reached when hardware blanking has failed.
    #[must_use]
    pub fn is_render(self) -> bool {
        matches!(self, StageKind::RenderBlack | StageKind::RenderScreensaver)
    }
}

impl serde::Serialize for StageKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            StageKind::Controller(mode) => mode.serialize(serializer),
            StageKind::RenderBlack => serializer.serialize_str("render_black"),
            StageKind::RenderScreensaver => serializer.serialize_str("render_screensaver"),
        }
    }
}

impl<'de> serde::Deserialize<'de> for StageKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        const EXPECTED: &[&str] = &[
            "power_off",
            "screen_off_audio_on",
            "brightness_zero",
            "render_black",
            "render_screensaver",
        ];

        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = StageKind;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a stage kind: \"power_off\", \"screen_off_audio_on\", \"brightness_zero\", \"render_black\", or \"render_screensaver\"")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<StageKind, E> {
                match s {
                    "power_off" => Ok(StageKind::Controller(BlankMode::PowerOff)),
                    "screen_off_audio_on" => Ok(StageKind::Controller(BlankMode::ScreenOffAudioOn)),
                    "brightness_zero" => Ok(StageKind::Controller(BlankMode::BrightnessZero)),
                    "render_black" => Ok(StageKind::RenderBlack),
                    "render_screensaver" => Ok(StageKind::RenderScreensaver),
                    _ => Err(E::unknown_variant(s, EXPECTED)),
                }
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

// ── Ladder stage ────────────────────────────────────────────────────────────

/// One rung of the escalation ladder: a stage kind and an optional dwell
/// (how long to stay in this stage before advancing).
///
/// When `dwell` is `None` the stage has no time limit — the ladder stays
/// here until an external event (sensor, user wake) moves it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LadderStage {
    /// What this rung does.
    pub kind: StageKind,
    /// How long to dwell at this stage before trying the next one.
    /// Deserialized with `humantime_serde::option`, so TOML values like
    /// `"5m"` or `"30s"` are accepted.  Absent when serialized to JSON
    /// (`skip_serializing_if`) rather than emitted as `null`.
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub dwell: Option<Duration>,
}

// ── Command failure ───────────────────────────────────────────────────────────

/// A structured record of a failed blank/wake command.
///
/// - `controller`: the name literal of the display controller that failed.
/// - `error`: a human-readable message that **starts with an `E_*` code constant**
///   from [`crate::error`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CmdFailure {
    /// Name of the display controller that failed.
    pub controller: String,
    /// Error detail, starting with an `E_*` code.
    pub error: String,
}

impl fmt::Display for CmdFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.controller, self.error)
    }
}

impl std::error::Error for CmdFailure {}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    // ── Serde stability ────────────────────────────────────────────────────

    #[test]
    fn blank_mode_serde_snake_case() {
        let json = serde_json::to_string(&BlankMode::ScreenOffAudioOn).unwrap();
        assert_eq!(json, "\"screen_off_audio_on\"");

        let deserialized: BlankMode = serde_json::from_str("\"screen_off_audio_on\"").unwrap();
        assert_eq!(deserialized, BlankMode::ScreenOffAudioOn);
    }

    #[test]
    fn blank_mode_power_off_serde() {
        let json = serde_json::to_string(&BlankMode::PowerOff).unwrap();
        assert_eq!(json, "\"power_off\"");

        let deserialized: BlankMode = serde_json::from_str("\"power_off\"").unwrap();
        assert_eq!(deserialized, BlankMode::PowerOff);
    }

    #[test]
    fn blank_mode_brightness_zero_serde() {
        let json = serde_json::to_string(&BlankMode::BrightnessZero).unwrap();
        assert_eq!(json, "\"brightness_zero\"");

        let deserialized: BlankMode = serde_json::from_str("\"brightness_zero\"").unwrap();
        assert_eq!(deserialized, BlankMode::BrightnessZero);
    }

    #[test]
    fn sensor_state_serde_lowercase() {
        let json = serde_json::to_string(&SensorState::Present).unwrap();
        assert_eq!(json, "\"present\"");

        let deserialized: SensorState = serde_json::from_str("\"present\"").unwrap();
        assert_eq!(deserialized, SensorState::Present);

        let json = serde_json::to_string(&SensorState::Unavailable).unwrap();
        assert_eq!(json, "\"unavailable\"");

        let deserialized: SensorState = serde_json::from_str("\"unavailable\"").unwrap();
        assert_eq!(deserialized, SensorState::Unavailable);
    }

    #[test]
    fn sensor_state_absent_serde_pin() {
        let json = serde_json::to_string(&SensorState::Absent).unwrap();
        assert_eq!(json, "\"absent\"");
    }

    #[test]
    fn presence_event_field_names_serde_pin() {
        let ev = PresenceEvent {
            sensor_id: SensorId("test".into()),
            state: SensorState::Present,
            confidence: 0.8,
            at: Timestamp(std::time::SystemTime::UNIX_EPOCH),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        let map = v.as_object().unwrap();
        assert!(map.contains_key("sensor_id"), "missing sensor_id");
        assert!(map.contains_key("state"), "missing state");
        assert!(map.contains_key("confidence"), "missing confidence");
        assert!(map.contains_key("at"), "missing at");
    }

    #[test]
    fn sensor_id_transparent_serde() {
        let id = SensorId("ld2410-usb".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"ld2410-usb\"");

        let deserialized: SensorId = serde_json::from_str("\"ld2410-usb\"").unwrap();
        assert_eq!(deserialized, id);
    }

    #[test]
    fn display_id_transparent_serde() {
        let id = DisplayId("kwin-dpms".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"kwin-dpms\"");

        let deserialized: DisplayId = serde_json::from_str("\"kwin-dpms\"").unwrap();
        assert_eq!(deserialized, id);
    }

    #[test]
    fn zone_id_transparent_serde() {
        let id = ZoneId("living-room".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"living-room\"");

        let deserialized: ZoneId = serde_json::from_str("\"living-room\"").unwrap();
        assert_eq!(deserialized, id);
    }

    #[test]
    fn rule_id_transparent_serde() {
        let id = RuleId("blank-after-5m".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"blank-after-5m\"");

        let deserialized: RuleId = serde_json::from_str("\"blank-after-5m\"").unwrap();
        assert_eq!(deserialized, id);
    }

    // ── Display impls ──────────────────────────────────────────────────────

    #[test]
    fn sensor_id_display_prints_inner() {
        let id = SensorId("ld2410-usb".into());
        assert_eq!(id.to_string(), "ld2410-usb");
    }

    #[test]
    fn display_id_display_prints_inner() {
        let id = DisplayId("kwin-dpms".into());
        assert_eq!(id.to_string(), "kwin-dpms");
    }

    #[test]
    fn zone_id_display_prints_inner() {
        let id = ZoneId("living-room".into());
        assert_eq!(id.to_string(), "living-room");
    }

    #[test]
    fn rule_id_display_prints_inner() {
        let id = RuleId("blank-after-5m".into());
        assert_eq!(id.to_string(), "blank-after-5m");
    }

    #[test]
    fn cmd_failure_display_format() {
        let f = CmdFailure {
            controller: "kwin-dpms".into(),
            error: format!("{}: timeout", crate::error::E_DISPLAY_IO),
        };
        assert_eq!(f.to_string(), "kwin-dpms: E_DISPLAY_IO: timeout");
    }

    // ── PresenceEvent::new ─────────────────────────────────────────────────

    #[test]
    fn presence_event_new_sets_confidence_one() {
        let sensor_id = SensorId("ld2410-usb".into());
        let at = Timestamp::now();
        let event = PresenceEvent::new(sensor_id.clone(), SensorState::Present, at);
        assert_eq!(event.sensor_id, sensor_id);
        assert_eq!(event.state, SensorState::Present);
        assert!((event.confidence - 1.0).abs() < f32::EPSILON);
    }

    // ── Timestamp / Tick ───────────────────────────────────────────────────

    #[test]
    fn timestamp_now_returns_recent_time() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ts = Timestamp::now();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ts_secs =
            ts.0.duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
        assert!(
            ts_secs >= before.as_secs() && ts_secs <= after.as_secs(),
            "timestamp should be between before and after"
        );
    }

    #[test]
    fn tick_now_returns_monotonic() {
        let t1 = Tick::now();
        let t2 = Tick::now();
        assert!(t2 >= t1);
    }

    #[test]
    fn tick_ordering() {
        let t1 = Tick(std::time::Instant::now());
        // Tick is Copy, so we can compare the same value
        assert_eq!(t1, t1);
        assert!(t1 <= t1);
        assert!(t1 >= t1);
    }
}

// ── StageKind / LadderStage ────────────────────────────────────────────

#[test]
fn stage_kind_serde_tags_are_flat() {
    assert_eq!(
        serde_json::to_string(&StageKind::Controller(BlankMode::PowerOff)).unwrap(),
        r#""power_off""#
    );
    assert_eq!(
        serde_json::to_string(&StageKind::Controller(BlankMode::ScreenOffAudioOn)).unwrap(),
        r#""screen_off_audio_on""#
    );
    assert_eq!(
        serde_json::to_string(&StageKind::Controller(BlankMode::BrightnessZero)).unwrap(),
        r#""brightness_zero""#
    );
    assert_eq!(
        serde_json::to_string(&StageKind::RenderBlack).unwrap(),
        r#""render_black""#
    );
    assert_eq!(
        serde_json::to_string(&StageKind::RenderScreensaver).unwrap(),
        r#""render_screensaver""#
    );
}

#[test]
fn stage_kind_roundtrips_all_five() {
    for k in [
        StageKind::Controller(BlankMode::PowerOff),
        StageKind::Controller(BlankMode::ScreenOffAudioOn),
        StageKind::Controller(BlankMode::BrightnessZero),
        StageKind::RenderBlack,
        StageKind::RenderScreensaver,
    ] {
        let s = serde_json::to_string(&k).unwrap();
        let back: StageKind = serde_json::from_str(&s).unwrap();
        assert_eq!(k, back);
    }
}

#[test]
fn stage_kind_is_render() {
    assert!(StageKind::RenderBlack.is_render());
    assert!(StageKind::RenderScreensaver.is_render());
    assert!(!StageKind::Controller(BlankMode::PowerOff).is_render());
}

#[test]
fn ladder_stage_dwell_parses_humantime() {
    let s: LadderStage = toml::from_str(
        r#"kind = "render_black"
dwell = "5m""#,
    )
    .unwrap();
    assert_eq!(s.kind, StageKind::RenderBlack);
    assert_eq!(s.dwell, Some(std::time::Duration::from_secs(300)));
    let t: LadderStage = toml::from_str(r#"kind = "power_off""#).unwrap();
    assert_eq!(t.dwell, None);
}
