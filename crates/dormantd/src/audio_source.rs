//! `PipeWire` audio/call inhibitor source.
//!
//! This module has two halves (spec §4):
//!
//! * The **pure** half (this commit): [`classify`] turns a `pw-dump` JSON
//!   dump into [`KindStates`] — which `InhibitorKind`s (`AudioPlayback`,
//!   `Call`) are currently active, per [`AudioConfig`]'s role/capture
//!   settings. No I/O, no subprocess, no async — `serde_json::Value`
//!   navigation only, so unknown fields are tolerated by construction
//!   (spec §9.5) and the classifier can be fixture-tested without a real
//!   `PipeWire` connection.
//! * The **async** half (poll loop, subprocess spawn, `startup_grace`,
//!   `min_active` debounce, circuit breaker) lands in a later commit next
//!   to this one.
//!
//! ## Classification rules (spec §4.2)
//!
//! Only nodes of `type == "PipeWire:Interface:Node"` whose
//! `info.props["media.class"]` is `"Stream/Output/Audio"` or
//! `"Stream/Input/Audio"` are considered; every other node (sinks,
//! sources, drivers, MIDI bridges, ports, links, …) carries no classifier
//! signal and is ignored.
//!
//! * A node's `info.state` must be running to count. **F5 (error
//!   granularity):** only the two recognized non-running states,
//!   `"idle"` and `"suspended"`, are treated as NOT running — a stream
//!   node with `state` missing or an unrecognized string is treated as
//!   RUNNING (classification uncertainty about a real stream fails
//!   toward keeping the screen on, never toward a whole-poll error).
//! * A running node whose `media.role` is in `cfg.call_roles` (default
//!   `["Communication"]`) → `call = true`, regardless of direction.
//! * Otherwise, a running `Stream/Input/Audio` (an open microphone) →
//!   `call = true` ONLY when `cfg.capture_is_call` is `true` (default
//!   `false`, F4 — `PipeWire` input nodes commonly sit `running` for hours
//!   under ordinary setups; a `true` default would silently defeat
//!   blanking for a wide slice of users).
//! * Otherwise, a running `Stream/Output/Audio` → `playback = true`,
//!   INCLUDING role-missing/unknown-role streams, UNLESS
//!   `cfg.playback_roles` is set and the role isn't in that list (a
//!   positive narrowing filter, opt-in only).
//!
//! `ClassifyError` is reserved for TOP-LEVEL JSON syntax failure and the
//! 4 MiB input cap ONLY (F5) — never for per-node anomalies.

use dormant_core::config::schema::AudioConfig;
use serde_json::Value;

/// Maximum accepted `pw-dump` stdout size (spec §4.3/§9.5): 4 MiB. Real
/// captures run ~200-300 KB (probe doc); this is a generous safety cap
/// against a pathologically busy `PipeWire` graph, not a realistic ceiling.
pub const MAX_INPUT_LEN: usize = 4 * 1024 * 1024;

/// Which inhibitor kinds the audio classifier currently sees as active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KindStates {
    /// A running, non-call output stream is present (`audio-playback`).
    pub playback: bool,
    /// A running stream classifies as a call (`call`).
    pub call: bool,
}

/// Top-level classification failure. Reserved for JSON syntax failure and
/// the 4 MiB cap ONLY (spec F5) — a well-formed node with an anomalous
/// sub-field is classified conservatively, never escalated to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifyError {
    /// The input was not valid JSON, or its top level was not an array of
    /// `pw-dump` objects.
    Json,
    /// The input exceeded [`MAX_INPUT_LEN`].
    TooLarge,
}

/// Classify a `pw-dump` JSON dump into [`KindStates`] per `cfg` (spec §4.2).
///
/// # Errors
///
/// Returns [`ClassifyError::TooLarge`] if `json` exceeds [`MAX_INPUT_LEN`],
/// or [`ClassifyError::Json`] if `json` fails to parse as a top-level JSON
/// array. Per-node anomalies (unrecognized `state`, missing `media.role`,
/// non-stream node shapes) never produce an `Err` — see the module docs.
pub fn classify(json: &str, cfg: &AudioConfig) -> Result<KindStates, ClassifyError> {
    if json.len() > MAX_INPUT_LEN {
        return Err(ClassifyError::TooLarge);
    }

    let root: Value = serde_json::from_str(json).map_err(|_| ClassifyError::Json)?;
    let nodes = root.as_array().ok_or(ClassifyError::Json)?;

    let mut states = KindStates::default();
    for node in nodes {
        classify_node(node, cfg, &mut states);
    }
    Ok(states)
}

/// Classify a single `pw-dump` object, folding its signal into `states`.
/// Non-Node objects and non-stream nodes are silently ignored (no
/// classifier signal lives there).
fn classify_node(node: &Value, cfg: &AudioConfig, states: &mut KindStates) {
    if node.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
        return;
    }
    let Some(info) = node.get("info") else {
        return;
    };
    let Some(props) = info.get("props") else {
        return;
    };
    let Some(media_class) = props.get("media.class").and_then(Value::as_str) else {
        return;
    };
    let is_input = media_class == "Stream/Input/Audio";
    let is_output = media_class == "Stream/Output/Audio";
    if !is_input && !is_output {
        return;
    }

    // F5: only the two recognized non-running states count as NOT
    // running; missing/unrecognized state is treated as running.
    let state = info.get("state").and_then(Value::as_str);
    if matches!(state, Some("idle" | "suspended")) {
        return;
    }

    let role = props
        .get("media.role")
        .and_then(Value::as_str)
        .unwrap_or("");
    if cfg.call_roles.iter().any(|r| r == role) {
        states.call = true;
        return;
    }
    if is_input {
        if cfg.capture_is_call {
            states.call = true;
        }
        return;
    }
    // Running, non-call output stream.
    match &cfg.playback_roles {
        Some(allowed) => {
            if allowed.iter().any(|r| r == role) {
                states.playback = true;
            }
        }
        None => states.playback = true,
    }
}

#[cfg(test)]
mod tests {
    use super::{ClassifyError, KindStates, classify};
    use dormant_core::config::schema::AudioConfig;

    const MOVIE: &str = include_str!("../tests/fixtures/pw_dump/movie.json");
    const MOVIE_PAUSED: &str = include_str!("../tests/fixtures/pw_dump/movie_paused.json");
    const CALL: &str = include_str!("../tests/fixtures/pw_dump/call.json");
    const IDLE: &str = include_str!("../tests/fixtures/pw_dump/idle.json");
    const MIC_ONLY: &str = include_str!("../tests/fixtures/pw_dump/mic_only.json");
    const IDLE_DIRTY: &str = include_str!("../tests/fixtures/pw_dump/idle_dirty.json");
    const ROLE_MISSING: &str = include_str!("../tests/fixtures/pw_dump/role_missing.json");
    const UNKNOWN_STATE: &str = include_str!("../tests/fixtures/pw_dump/unknown_state.json");
    const MUSIC: &str = include_str!("../tests/fixtures/pw_dump/music.json");

    fn default_cfg() -> AudioConfig {
        AudioConfig::default()
    }

    #[test]
    fn movie_running_output_is_playback_not_call() {
        let states = classify(MOVIE, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: true,
                call: false
            }
        );
    }

    /// Documented plan/fixture drift (see fixtures README): the plan's T4
    /// Step 1 text claims `call.json` + default `call_roles` yields
    /// `call=true`. The real capture's only running input (pw-record) has
    /// `media.role="Music"`, not `"Communication"` — under the default
    /// config this fixture classifies identically to `movie.json`
    /// (`playback=true, call=false`), matching the probe doc's own
    /// state->signal table (`call-standin | true | false | true`). This
    /// test asserts the REAL fixture content, not the plan's stale claim.
    #[test]
    fn call_fixture_under_default_config_is_playback_not_call() {
        let states = classify(CALL, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: true,
                call: false
            }
        );
    }

    #[test]
    fn movie_paused_is_neither() {
        let states = classify(MOVIE_PAUSED, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: false,
                call: false
            }
        );
    }

    #[test]
    fn idle_is_neither() {
        let states = classify(IDLE, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: false,
                call: false
            }
        );
    }

    /// F4 pin: a running microphone stream does NOT count as a call under
    /// the default `capture_is_call = false`.
    #[test]
    fn mic_only_call_false_under_default_capture_is_call() {
        let states = classify(MIC_ONLY, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: false,
                call: false
            }
        );
    }

    /// F4 pin, opt-in half: enabling `capture_is_call` makes the same
    /// running microphone stream count as a call.
    #[test]
    fn mic_only_call_true_when_capture_is_call_enabled() {
        let cfg = AudioConfig {
            capture_is_call: true,
            ..default_cfg()
        };
        let states = classify(MIC_ONLY, &cfg).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: false,
                call: true
            }
        );
    }

    /// Role-missing running output ⇒ playback (spec §4.2: "INCLUDING
    /// role-missing/unknown-role streams"), pinned in isolation via the
    /// single-node derivative (see fixtures README).
    #[test]
    fn role_missing_running_output_is_playback() {
        let states = classify(ROLE_MISSING, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: true,
                call: false
            }
        );
    }

    /// F5 pin: a stream-class node with an unrecognized `state` string
    /// (not "running"/"idle"/"suspended") is treated as RUNNING.
    #[test]
    fn unknown_state_stream_node_is_treated_as_running() {
        let states = classify(UNKNOWN_STATE, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: true,
                call: false
            }
        );
    }

    /// Default `playback_roles` (unset) is permissive: the `music.json`
    /// derivative (role="Music") still inhibits when no narrowing is set.
    #[test]
    fn music_role_inhibits_playback_by_default() {
        let states = classify(MUSIC, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: true,
                call: false
            }
        );
    }

    /// F16 pin: `playback_roles = Some(["Movie"])` narrows — a running
    /// output whose role is "Music" (not in the allowed list) must NOT
    /// inhibit.
    #[test]
    fn playback_roles_narrowing_excludes_music() {
        let cfg = AudioConfig {
            playback_roles: Some(vec!["Movie".to_string()]),
            ..default_cfg()
        };
        let states = classify(MUSIC, &cfg).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: false,
                call: false
            }
        );
    }

    /// Orphan-stream edge case (README): a dead process's node can still
    /// report `state=running`. The classifier performs no process-liveness
    /// check — it tolerates the false positive by design (fail toward not
    /// blanking), identically to a real running stream.
    #[test]
    fn idle_dirty_orphan_node_is_tolerated_as_playback() {
        let states = classify(IDLE_DIRTY, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: true,
                call: false
            }
        );
    }

    /// Synthetic minimal JSON (NOT a stored/captured fixture — no probe
    /// capture ever observed `media.role == "Communication"` in practice;
    /// see the fixtures README's documented plan/fixture drift). Pins the
    /// role-based call branch of `classify()` directly against spec §4.2,
    /// since no real capture exercises it.
    #[test]
    fn running_output_with_communication_role_is_call_not_playback() {
        let json = r#"[
            {
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "state": "running",
                    "props": {
                        "media.class": "Stream/Output/Audio",
                        "media.role": "Communication"
                    }
                }
            }
        ]"#;
        let states = classify(json, &default_cfg()).unwrap();
        assert_eq!(
            states,
            KindStates {
                playback: false,
                call: true
            }
        );
    }

    #[test]
    fn top_level_garbage_is_json_error() {
        let err = classify("not valid json {{{", &default_cfg()).unwrap_err();
        assert_eq!(err, ClassifyError::Json);
    }

    #[test]
    fn oversized_input_is_too_large_error() {
        let huge = "a".repeat(super::MAX_INPUT_LEN + 1);
        let err = classify(&huge, &default_cfg()).unwrap_err();
        assert_eq!(err, ClassifyError::TooLarge);
    }
}
