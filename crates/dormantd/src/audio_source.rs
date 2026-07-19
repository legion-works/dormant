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
//! * The **async** half (`run_loop`): the poll loop, subprocess
//!   spawn/capture, bounded reap list, and transition publication. Timing,
//!   debounce, failure, and breaker decisions live in [`crate::audio_policy`].
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

// ─────────────────────────────────────────────────────────────────────────
// The ASYNC half (spec §4.3): subprocess-spawn poll loop, bounded reap list,
// and transition publication.
//
// Mirrors `idle_source.rs`'s `dbus_run` shape: per-function `cfg` gating
// with a park-on-cancel stub off Linux (`idle_source.rs:289-298`), chosen
// over a whole-module gate because this poller lives in-crate next to
// `idle_source` (spec §2/F12).
// ─────────────────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crate::audio_policy::{AudioPolicy, AudioTransition, ProbeOutcome};
use dormant_core::rules::{ControlMsg, InhibitorKind};
use dormant_core::types::RuleId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One rule that declares an audio-related inhibitor kind (`"audio-playback"`
/// and/or `"call"`). Built by `dormantd::app::audio_rules` (spec §4.3),
/// which filters `RuleConfig::inhibitors` via [`InhibitorKind::from_config`]
/// — never raw string literals (F7).
#[derive(Debug, Clone)]
pub struct AudioRule {
    /// The rule this inhibitor gates.
    pub rule: RuleId,
    /// Which audio-related kinds this rule declared.
    pub kinds: Vec<InhibitorKind>,
}

/// Passive config/wiring for the audio poller.
///
/// **Deliberately does NOT own the reap list** (cold-gate review, rev-1
/// Finding 1): the reap list is local mutable state declared once inside
/// `run_loop`'s own body, not interior-mutable state inside this struct.
/// `run_loop` is a single `async fn` owning its own loop — no other task
/// reads or writes the reap list, so no `Arc<Mutex<_>>` is needed; every
/// existing `run`/`dbus_run` loop in `idle_source.rs` manages its own local
/// mutable state (e.g. `last_sent`) the same way.
pub struct AudioDeps {
    /// Channel to the rules engine.
    pub ctl: mpsc::Sender<ControlMsg>,
    /// The global `[audio]` config section.
    pub cfg: AudioConfig,
    /// Rules that opted into an audio-related inhibitor kind.
    pub rules: Vec<AudioRule>,
}

/// Outcome of the post-kill reap wait — the test seam (R3-M/C).
///
/// No portable test fixture can produce a genuinely SIGKILL-immune (D-state)
/// child process, so the breaker-trip path is pinned at the unit level
/// through this injectable probe instead: production performs the real
/// 1s-bounded `wait()`; tests script `Unreapable` per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReapOutcome {
    /// The child was reaped within the post-kill bound.
    Reaped,
    /// The child did not reap within the bound — moved to the reap list.
    Unreapable,
}

/// Boxed-closure probe seam kept non-generic on purpose (the loop function
/// stays a plain `async fn`, not generic over a trait).
pub(crate) type ReapProbe = Box<
    dyn FnMut(&mut tokio::process::Child) -> Pin<Box<dyn Future<Output = ReapOutcome> + Send + '_>>
        + Send,
>;

/// Bounded reap-list capacity (spec §4.3/§6#5) — the circuit breaker trips
/// when it fills; a chronic wedge costs at most this many outstanding
/// orphans, never one per poll tick.
const REAP_CAP: usize = 8;

/// Spawn-retry cadence while the breaker is tripped (the
/// `DBUS_RECONNECT_INTERVAL` class, spec §4.3). Shrunk under `#[cfg(test)]`
/// so the breaker-recovery poller test doesn't block on a real 60s wait —
/// the production value is unchanged.
#[cfg(not(test))]
const BREAKER_RETRY_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(test)]
const BREAKER_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Construct the production [`ReapProbe`]: `start_kill()` followed by its
/// own 1s-bounded `wait()` (F3/R2-M2 — diverges from `command.rs:159-176`'s
/// unbounded post-kill wait deliberately).
pub(crate) fn production_reap_probe() -> ReapProbe {
    Box::new(|child: &mut tokio::process::Child| {
        Box::pin(async move {
            let _ = child.start_kill();
            match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                Ok(_) => ReapOutcome::Reaped,
                Err(_) => ReapOutcome::Unreapable,
            }
        })
    })
}

/// Coarse failure categorization for one tick — drives `consecutive_failures`
/// and the warn-once log events (F11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// `Command::spawn` itself failed (e.g. missing binary — P10).
    SpawnFailed,
    /// The whole tick exceeded `poll_interval` (P1).
    TimedOut,
    /// The process exited with a non-zero status.
    NonZeroExit,
    /// Stdout exceeded [`MAX_INPUT_LEN`] before EOF (P2).
    TooLarge,
}

/// Result of one poll tick's subprocess phase (pre-classification).
enum TickOutcome {
    /// Captured stdout, ready for [`classify`].
    Success(String),
    /// The tick failed before classification could run.
    Failure(FailureKind),
}

/// Run one `pw-dump` invocation: spawn, capped-read stdout (4 MiB, §4.3),
/// drain stderr concurrently (`command.rs:11-24` rationale), bound the whole
/// tick by `poll_interval`. On timeout or cap-exceeded, hands the child to
/// `probe` to decide reap vs. push onto `reap_list` (still local to
/// `run_loop`, passed through here so a stuck tick's child can join it).
/// Outcome of the capped-read + wait phase, before the outer `poll_interval`
/// timeout in [`run_tick`] is applied.
#[cfg(target_os = "linux")]
enum TickBody {
    Oversized,
    Exited(std::process::ExitStatus, Vec<u8>),
    WaitErr,
}

#[cfg(target_os = "linux")]
async fn run_tick(
    command: &str,
    poll_interval: Duration,
    probe: &mut ReapProbe,
    reap_list: &mut Vec<tokio::process::Child>,
) -> TickOutcome {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return TickOutcome::Failure(FailureKind::SpawnFailed);
    };
    let args: Vec<&str> = parts.collect();

    let Ok(mut child) = tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    else {
        return TickOutcome::Failure(FailureKind::SpawnFailed);
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Concurrent stderr drain (command.rs:11-24): an unread OS pipe buffer
    // would otherwise deadlock the child's write(2) and wedge our wait().
    let stderr_drain = stderr.map(|mut pipe| {
        tokio::spawn(async move {
            let mut chunk = [0u8; 1024];
            loop {
                match tokio::io::AsyncReadExt::read(&mut pipe, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
    });

    let body = async {
        let mut buf: Vec<u8> = Vec::new();
        if let Some(mut pipe) = stdout {
            let mut chunk = [0u8; 8192];
            loop {
                match tokio::io::AsyncReadExt::read(&mut pipe, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > MAX_INPUT_LEN {
                            // Stop reading immediately — do not wait for
                            // EOF or the outer timeout (P2: bounded read).
                            return TickBody::Oversized;
                        }
                    }
                }
            }
        }
        match child.wait().await {
            Ok(status) => TickBody::Exited(status, buf),
            Err(_) => TickBody::WaitErr,
        }
    };

    match tokio::time::timeout(poll_interval, body).await {
        Err(_elapsed) => {
            if matches!(probe(&mut child).await, ReapOutcome::Unreapable) {
                reap_list.push(child);
            }
            if let Some(h) = stderr_drain {
                h.abort();
            }
            TickOutcome::Failure(FailureKind::TimedOut)
        }
        Ok(TickBody::Oversized) => {
            if matches!(probe(&mut child).await, ReapOutcome::Unreapable) {
                reap_list.push(child);
            }
            if let Some(h) = stderr_drain {
                h.abort();
            }
            TickOutcome::Failure(FailureKind::TooLarge)
        }
        Ok(TickBody::WaitErr) => TickOutcome::Failure(FailureKind::TimedOut),
        Ok(TickBody::Exited(status, buf)) => {
            if let Some(h) = stderr_drain {
                let _ = h.await;
            }
            if status.success() {
                TickOutcome::Success(String::from_utf8_lossy(&buf).into_owned())
            } else {
                TickOutcome::Failure(FailureKind::NonZeroExit)
            }
        }
    }
}

/// Send a `SetInhibited` only when the value changed for this `(rule, kind)`
/// pair, recording the new value only on a successful `try_send` (dropped
/// edges are retried next tick — `idle_source.rs:316-334` precedent).
#[cfg(target_os = "linux")]
fn publish(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<(RuleId, InhibitorKind), bool>,
    rule: &RuleId,
    kind: InhibitorKind,
    inhibited: bool,
) {
    let key = (rule.clone(), kind);
    if last_sent.get(&key) == Some(&inhibited) {
        return;
    }
    if ctl
        .try_send(ControlMsg::SetInhibited {
            rule: Some(rule.clone()),
            kind,
            inhibited,
        })
        .is_ok()
    {
        last_sent.insert(key, inhibited);
    }
}

/// Apply the policy's desired inhibition edges to each declared audio rule.
#[cfg(target_os = "linux")]
fn publish_transitions(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<(RuleId, InhibitorKind), bool>,
    rules: &[AudioRule],
    transitions: &[AudioTransition],
) {
    for transition in transitions {
        let (kind, inhibited) = match transition {
            AudioTransition::Assert(kind) => (*kind, true),
            AudioTransition::Deassert(kind) => (*kind, false),
            AudioTransition::OpenBreaker
            | AudioTransition::CloseBreaker
            | AudioTransition::NoChange => {
                continue;
            }
        };
        for rule in rules {
            if rule.kinds.contains(&kind) {
                publish(ctl, last_sent, &rule.rule, kind, inhibited);
            }
        }
    }
}

/// Sleep for `dur` or return `true` if cancellation fired first
/// (`idle_source.rs:353-359` precedent).
#[cfg(target_os = "linux")]
async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(dur) => false,
    }
}

/// Run the audio/call inhibitor poll loop (spec §4.3).
///
/// `deps` is passive config/wiring only; the bounded reap list is LOCAL
/// mutable state declared once here, before the tick loop starts (cold-gate
/// pin, rev-1 Finding 1) — `run_loop` is a single `async fn` owning its own
/// loop, so no `Arc<Mutex<_>>` is needed. Tests call this directly with an
/// injected [`ReapProbe`]; [`crate::inhibit_audio::spawn`] is the
/// probe-free public surface that constructs the production probe.
#[cfg(target_os = "linux")]
pub(crate) async fn run_loop(deps: AudioDeps, mut probe: ReapProbe, cancel: CancellationToken) {
    let AudioDeps { ctl, cfg, rules } = deps;
    let mut reap_list: Vec<tokio::process::Child> = Vec::new();
    let mut last_sent: HashMap<(RuleId, InhibitorKind), bool> = HashMap::new();
    let mut policy = AudioPolicy::new(cfg.min_active, BREAKER_RETRY_INTERVAL);
    let mut warned_unreachable = false;
    let mut warned_parse_failed = false;

    tracing::info!(event = "audio_inhibitor_started");

    loop {
        // Per-tick housekeeping: reap any orphans that have since exited on
        // their own (never zombies — spec §6#5).
        reap_list.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));

        if reap_list.len() >= REAP_CAP {
            let now = Instant::now();
            let transitions = policy.step(now, ProbeOutcome::ReapCapReached);
            if transitions.contains(&AudioTransition::OpenBreaker) {
                tracing::error!(
                    event = "audio_inhibitor_disabled",
                    "reap list full ({REAP_CAP} outstanding); pausing pw-dump spawns",
                );
            }
            publish_transitions(&ctl, &mut last_sent, &rules, &transitions);
            let cooldown = policy
                .cooldown_remaining(now)
                .unwrap_or(BREAKER_RETRY_INTERVAL);
            if sleep_or_cancel(cooldown, &cancel).await {
                return;
            }
            continue;
        }

        match run_tick(
            &cfg.pw_dump_command,
            cfg.poll_interval,
            &mut probe,
            &mut reap_list,
        )
        .await
        {
            TickOutcome::Success(json) => {
                if let Ok(states) = classify(&json, &cfg) {
                    warned_unreachable = false;
                    warned_parse_failed = false;
                    let now = Instant::now();
                    let transitions = policy.step(
                        now,
                        ProbeOutcome::Classified {
                            playback: states.playback,
                            call: states.call,
                        },
                    );
                    publish_transitions(&ctl, &mut last_sent, &rules, &transitions);
                } else {
                    if !warned_parse_failed {
                        tracing::warn!(event = "audio_parse_failed");
                        warned_parse_failed = true;
                    }
                    let now = Instant::now();
                    let transitions = policy.step(now, ProbeOutcome::Failure);
                    publish_transitions(&ctl, &mut last_sent, &rules, &transitions);
                }
            }
            TickOutcome::Failure(kind) => {
                if kind == FailureKind::TimedOut {
                    tracing::warn!(event = "audio_poll_overrun");
                }
                if !warned_unreachable {
                    tracing::warn!(event = "audio_inhibitor_unreachable");
                    warned_unreachable = true;
                }
                let now = Instant::now();
                let transitions = policy.step(now, ProbeOutcome::Failure);
                publish_transitions(&ctl, &mut last_sent, &rules, &transitions);
            }
        }

        if sleep_or_cancel(cfg.poll_interval, &cancel).await {
            return;
        }
    }
}

/// Non-Linux stub — `pw-dump` does not exist off Linux (spec §2,
/// `idle_source.rs:289-298` per-function-cfg precedent).
#[cfg(not(target_os = "linux"))]
pub(crate) async fn run_loop(_deps: AudioDeps, _probe: ReapProbe, cancel: CancellationToken) {
    cancel.cancelled().await;
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

/// Linux process contracts for the poller I/O boundary.
#[cfg(all(test, target_os = "linux"))]
mod poller_tests {
    use super::{
        AudioDeps, AudioRule, FailureKind, TickOutcome, production_reap_probe, run_loop, run_tick,
    };
    use dormant_core::config::schema::AudioConfig;
    use dormant_core::rules::InhibitorKind;
    use dormant_core::types::RuleId;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    const MOVIE: &str = include_str!("../tests/fixtures/pw_dump/movie.json");
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn cat_script(json: &str) -> String {
        format!("#!/bin/sh\ncat <<'AUDIO_FIXTURE_EOF'\n{json}\nAUDIO_FIXTURE_EOF\n")
    }

    fn rule(id: &str, kinds: &[InhibitorKind]) -> AudioRule {
        AudioRule {
            rule: RuleId(id.to_string()),
            kinds: kinds.to_vec(),
        }
    }

    fn cfg(command: &Path, poll_interval: Duration, min_active: Duration) -> AudioConfig {
        AudioConfig {
            poll_interval,
            min_active,
            pw_dump_command: command.to_string_lossy().into_owned(),
            ..AudioConfig::default()
        }
    }

    // ── no_rules_spawns_nothing ───────────────────────────────────────────────

    #[tokio::test]
    async fn no_rules_spawns_nothing() {
        let (ctl, _ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let result = crate::inhibit_audio::spawn(vec![], AudioConfig::default(), ctl, cancel);
        assert!(result.is_none(), "no audio rules must spawn nothing");
    }

    #[tokio::test]
    async fn successful_probe_exits_before_its_outcome_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", &cat_script(MOVIE));
        let mut reap_list = Vec::new();
        let mut probe = production_reap_probe();

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            run_tick(
                &script.to_string_lossy(),
                Duration::from_secs(1),
                &mut probe,
                &mut reap_list,
            ),
        )
        .await
        .expect("successful probe must exit");

        assert!(matches!(outcome, TickOutcome::Success(_)));
        assert!(
            reap_list.is_empty(),
            "an exited child must not enter the reap list"
        );
    }

    #[tokio::test]
    async fn timed_out_probe_is_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", "#!/bin/sh\nexec sleep 100000\n");
        let mut reap_list = Vec::new();
        let mut probe = production_reap_probe();

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            run_tick(
                &script.to_string_lossy(),
                Duration::from_millis(80),
                &mut probe,
                &mut reap_list,
            ),
        )
        .await
        .expect("timed-out probe must return after kill");

        assert!(matches!(
            outcome,
            TickOutcome::Failure(FailureKind::TimedOut)
        ));
        assert!(reap_list.is_empty(), "the killed child must be reaped");
    }

    #[tokio::test]
    async fn cancellation_stops_the_poller_after_a_completed_probe() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", &cat_script(MOVIE));
        let (ctl, _ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, Duration::from_secs(1), Duration::ZERO),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cancellation must stop the poller")
            .expect("poller task must not panic");
    }
}
