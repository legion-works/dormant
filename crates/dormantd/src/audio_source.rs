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
//!   spawn/capture, `startup_grace`, `min_active`/`active_since` debounce,
//!   the `consecutive_failures` transient policy, and the bounded reap list
//!   + circuit breaker (spec §4.3).
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
// The ASYNC half (spec §4.3): subprocess-spawn poll loop, startup_grace,
// min_active/active_since debounce, consecutive_failures transient policy,
// bounded reap list + circuit breaker, publish/dedup.
//
// Mirrors `idle_source.rs`'s `dbus_run` shape: per-function `cfg` gating
// with a park-on-cancel stub off Linux (`idle_source.rs:289-298`), chosen
// over a whole-module gate because this poller lives in-crate next to
// `idle_source` (spec §2/F12).
// ─────────────────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

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

/// Per-kind debounce bookkeeping (spec §4.3: `active_since` + the derived
/// `effective` bit that gets published).
#[derive(Debug, Default, Clone, Copy)]
struct KindDebounce {
    /// When continuous activity for this kind began (tokio's mockable
    /// clock, so paused-clock tests can drive it deterministically).
    active_since: Option<tokio::time::Instant>,
    /// The currently-published-worthy effective value for this kind.
    effective: bool,
}

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

/// Fold one successful classification into the per-kind debounce state.
///
/// `startup_grace`: on the FIRST successful classification, a running
/// stream asserts WITHOUT waiting `min_active` (F2/R2-M1 — its prior
/// duration is unknown; `min_active` debounces NEW blips, not pre-existing
/// streams). Deassertion is always immediate (asymmetric on purpose).
///
/// `entry.effective` is monotonic while `active` stays continuously true:
/// once asserted (via the `startup_grace` exemption OR by `min_active`
/// having elapsed), it STAYS asserted -- it is never recomputed back to
/// `false` just because a later tick's `elapsed_since(active_since)` is
/// smaller than `min_active` would otherwise require. Only the `active =
/// false` branch below (an actual deassert) or the transient-failure reset
/// (`reset_debounce`, spec F11) ever clears it. Without the `entry.effective
/// ||` latch, a stream that asserts via `startup_grace` on tick 1 would
/// spuriously flip back to unasserted on tick 2 (since `elapsed_since
/// (active_since) < min_active` still holds one poll cycle later) and only
/// re-assert once `min_active` naturally elapsed -- re-opening exactly the
/// reload-mid-movie window the exemption exists to close whenever
/// `min_active > poll_interval` (`daemon_smoke`'s reload-mid-movie anti-
/// tautology test, T6, caught this: grace could expire in the false-flip
/// gap before the natural reassert).
#[cfg(target_os = "linux")]
fn apply_success(
    states: KindStates,
    startup_grace: bool,
    min_active: Duration,
    debounce: &mut HashMap<InhibitorKind, KindDebounce>,
) {
    let now = tokio::time::Instant::now();
    for (kind, active) in [
        (InhibitorKind::AudioPlayback, states.playback),
        (InhibitorKind::Call, states.call),
    ] {
        let entry = debounce.entry(kind).or_default();
        if active {
            if entry.active_since.is_none() {
                entry.active_since = Some(now);
            }
            entry.effective = entry.effective
                || startup_grace
                || now.saturating_duration_since(entry.active_since.unwrap_or(now)) >= min_active;
        } else {
            entry.active_since = None;
            entry.effective = false;
        }
    }
}

/// Reset both kinds to inactive (transient-failure policy §4.3/F11, and the
/// circuit breaker) — does NOT touch `startup_grace`, which only a
/// successful classification consumes (R3-S/B).
#[cfg(target_os = "linux")]
fn reset_debounce(debounce: &mut HashMap<InhibitorKind, KindDebounce>) {
    for v in debounce.values_mut() {
        v.active_since = None;
        v.effective = false;
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

/// Publish each rule's declared kinds at their current debounced effective
/// value (F10: dedup keyed `HashMap<(RuleId, InhibitorKind), bool>`).
#[cfg(target_os = "linux")]
fn publish_effective(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<(RuleId, InhibitorKind), bool>,
    rules: &[AudioRule],
    debounce: &HashMap<InhibitorKind, KindDebounce>,
) {
    for r in rules {
        for &k in &r.kinds {
            let v = debounce.get(&k).is_some_and(|d| d.effective);
            publish(ctl, last_sent, &r.rule, k, v);
        }
    }
}

/// Publish `false` for every declared kind of every rule (fail-toward-blanking).
#[cfg(target_os = "linux")]
fn publish_inactive_all(
    ctl: &mpsc::Sender<ControlMsg>,
    last_sent: &mut HashMap<(RuleId, InhibitorKind), bool>,
    rules: &[AudioRule],
) {
    for r in rules {
        for &k in &r.kinds {
            publish(ctl, last_sent, &r.rule, k, false);
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
    let mut debounce: HashMap<InhibitorKind, KindDebounce> = HashMap::from([
        (InhibitorKind::AudioPlayback, KindDebounce::default()),
        (InhibitorKind::Call, KindDebounce::default()),
    ]);
    let mut consecutive_failures: u32 = 0;
    let mut startup_grace = true;
    let mut warned_unreachable = false;
    let mut warned_parse_failed = false;
    let mut breaker_tripped = false;

    tracing::info!(event = "audio_inhibitor_started");

    loop {
        // Per-tick housekeeping: reap any orphans that have since exited on
        // their own (never zombies — spec §6#5).
        reap_list.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));

        if reap_list.len() >= REAP_CAP {
            if !breaker_tripped {
                tracing::error!(
                    event = "audio_inhibitor_disabled",
                    "reap list full ({REAP_CAP} outstanding); pausing pw-dump spawns",
                );
                breaker_tripped = true;
            }
            publish_inactive_all(&ctl, &mut last_sent, &rules);
            reset_debounce(&mut debounce);
            if sleep_or_cancel(BREAKER_RETRY_INTERVAL, &cancel).await {
                return;
            }
            continue;
        }
        breaker_tripped = false;

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
                    consecutive_failures = 0;
                    warned_unreachable = false;
                    warned_parse_failed = false;
                    apply_success(states, startup_grace, cfg.min_active, &mut debounce);
                    startup_grace = false;
                    publish_effective(&ctl, &mut last_sent, &rules, &debounce);
                } else {
                    if !warned_parse_failed {
                        tracing::warn!(event = "audio_parse_failed");
                        warned_parse_failed = true;
                    }
                    consecutive_failures += 1;
                    if consecutive_failures >= 2 {
                        publish_inactive_all(&ctl, &mut last_sent, &rules);
                        reset_debounce(&mut debounce);
                    }
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
                consecutive_failures += 1;
                if consecutive_failures >= 2 {
                    publish_inactive_all(&ctl, &mut last_sent, &rules);
                    reset_debounce(&mut debounce);
                }
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

/// Poller tests (spec §4.3/§7) — the ASYNC half. Fake scripts are written at
/// test setup and `chmod`'d 0o755 BEFORE any rename (the `write_credentials`
/// `PermissionsExt` precedent, `daemon_smoke.rs:146-153` — same mechanism,
/// 0o755 not 0o600, because the script must be executable). Phase swaps use
/// write-temp + chmod + atomic rename (F13).
#[cfg(test)]
mod poller_tests {
    use super::{AudioDeps, AudioRule, ReapOutcome, production_reap_probe, run_loop};
    use dormant_core::config::schema::AudioConfig;
    use dormant_core::rules::{ControlMsg, InhibitorKind};
    use dormant_core::types::RuleId;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    const MOVIE: &str = include_str!("../tests/fixtures/pw_dump/movie.json");
    const IDLE: &str = include_str!("../tests/fixtures/pw_dump/idle.json");

    /// Two-node synthetic JSON (not a stored fixture — mirrors the pure
    /// classifier test's own precedent for role-based cases no probe
    /// capture exercises): a Communication-role output (call) alongside a
    /// role-less output (playback), so both kinds are simultaneously true.
    const BOTH_KINDS: &str = r#"[
        {
            "id": 1,
            "type": "PipeWire:Interface:Node",
            "info": {
                "state": "running",
                "props": { "media.class": "Stream/Output/Audio", "media.role": "Communication" }
            }
        },
        {
            "id": 2,
            "type": "PipeWire:Interface:Node",
            "info": {
                "state": "running",
                "props": { "media.class": "Stream/Output/Audio" }
            }
        }
    ]"#;

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

    /// Atomically rewrite a script's contents: write to a sibling temp file,
    /// `chmod` 0o755 BEFORE renaming (R2-N — rename carries the temp's mode;
    /// losing the exec bit silently flips the test into the failure path),
    /// then rename over the original.
    fn rewrite_script(path: &Path, body: &str) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::rename(&tmp, path).unwrap();
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

    /// Drain every `SetInhibited` currently queued on `rx` without blocking.
    fn drain(rx: &mut mpsc::Receiver<ControlMsg>) -> Vec<(RuleId, InhibitorKind, bool)> {
        let mut out = Vec::new();
        while let Ok(ControlMsg::SetInhibited {
            rule: Some(r),
            kind,
            inhibited,
        }) = rx.try_recv()
        {
            out.push((r, kind, inhibited));
        }
        out
    }

    /// Wait until `pred` observes at least one matching message, or time out.
    async fn wait_for_msg(
        rx: &mut mpsc::Receiver<ControlMsg>,
        timeout: Duration,
        pred: impl Fn(&RuleId, InhibitorKind, bool) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ControlMsg::SetInhibited {
                    rule: Some(r),
                    kind,
                    inhibited,
                })) => {
                    if pred(&r, kind, inhibited) {
                        return true;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return false,
            }
        }
    }

    /// Assert no message matching `pred` arrives within `window`.
    async fn assert_no_msg(
        rx: &mut mpsc::Receiver<ControlMsg>,
        window: Duration,
        pred: impl Fn(&RuleId, InhibitorKind, bool) -> bool,
    ) {
        let saw = wait_for_msg(rx, window, pred).await;
        assert!(!saw, "unexpected matching SetInhibited within {window:?}");
    }

    /// A `ReapProbe` that always reports `Unreapable` without touching the
    /// child at all — the underlying (real) process keeps running
    /// undisturbed. This is the R3-M/C seam: no portable fixture can produce
    /// a genuinely SIGKILL-immune child, so the test controls the decision
    /// directly instead of relying on OS behavior.
    fn always_unreapable_probe() -> super::ReapProbe {
        Box::new(|_child: &mut tokio::process::Child| {
            Box::pin(async move { ReapOutcome::Unreapable })
        })
    }

    // ── startup_asserts_immediately_on_running_stream ───────────────────────

    #[tokio::test]
    async fn startup_asserts_immediately_on_running_stream() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", &cat_script(MOVIE));
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        // min_active is set far larger than a single tick's real overhead
        // (~100ms in this sandbox) so ordinary debounce -- which would need
        // a FRESH min_active to elapse since active_since before asserting
        // -- cannot possibly land inside the tight deadline below; only the
        // startup_grace exemption's immediate assert can.
        let min_active = Duration::from_millis(2500);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, Duration::from_millis(500), min_active),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let start = tokio::time::Instant::now();
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        let asserted = wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
            k == InhibitorKind::AudioPlayback && v
        })
        .await;
        assert!(
            asserted,
            "first tick on a running stream must assert immediately, without waiting min_active"
        );
        assert!(
            start.elapsed() < min_active,
            "assert must land well before a full min_active window elapses"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── startup_grace_survives_leading_failures ──────────────────────────────

    #[tokio::test]
    async fn startup_grace_survives_leading_failures() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", "#!/bin/sh\nexit 1\n");
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        // min_active is set far larger than a single tick's real overhead
        // so ordinary debounce (a fresh min_active wait since active_since)
        // cannot land inside the tight deadline below -- only startup_grace,
        // never consumed by the two leading failures, can.
        let min_active = Duration::from_millis(2500);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, Duration::from_millis(60), min_active),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // Two failing ticks, then swap to a running stream.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let swap_at = tokio::time::Instant::now();
        rewrite_script(&script, &cat_script(MOVIE));

        let asserted = wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
            k == InhibitorKind::AudioPlayback && v
        })
        .await;
        assert!(
            asserted,
            "startup_grace must survive leading failures and assert on first success"
        );
        assert!(
            swap_at.elapsed() < min_active,
            "assert must land well before a full min_active window elapses"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── min_active_debounces_new_blips ───────────────────────────────────────

    #[tokio::test]
    async fn min_active_debounces_new_blips() {
        let dir = tempfile::tempdir().unwrap();
        // Startup tick sees idle (consumes startup_grace with no effect).
        let script = write_script(dir.path(), "pw-dump", &cat_script(IDLE));
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(40);
        let min_active = Duration::from_millis(300);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, min_active),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // Let the startup tick land (idle, no assert).
        tokio::time::sleep(Duration::from_millis(100)).await;
        drain(&mut ctl_rx);

        // Blip: movie for well under min_active, then back to idle.
        rewrite_script(&script, &cat_script(MOVIE));
        tokio::time::sleep(Duration::from_millis(120)).await;
        rewrite_script(&script, &cat_script(IDLE));

        // Across the whole blip and afterward, no assert must ever fire.
        assert_no_msg(&mut ctl_rx, Duration::from_millis(400), |_, k, v| {
            k == InhibitorKind::AudioPlayback && v
        })
        .await;

        cancel.cancel();
        let _ = handle.await;
    }

    // ── deassert_is_immediate ─────────────────────────────────────────────

    #[tokio::test]
    async fn deassert_is_immediate() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", &cat_script(MOVIE));
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(40);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, Duration::from_millis(100)),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // Startup asserts immediately (running stream on tick 1).
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::AudioPlayback && v
            })
            .await
        );

        rewrite_script(&script, &cat_script(IDLE));

        // Deassert must land within roughly one poll cycle — no min_active wait.
        let deasserted = wait_for_msg(&mut ctl_rx, Duration::from_millis(400), |_, k, v| {
            k == InhibitorKind::AudioPlayback && !v
        })
        .await;
        assert!(deasserted, "deassert must be immediate on the next tick");

        cancel.cancel();
        let _ = handle.await;
    }

    // ── one_failure_keeps_state_two_reset ────────────────────────────────────

    #[tokio::test]
    async fn one_failure_keeps_state_two_reset() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", &cat_script(BOTH_KINDS));
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(50);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, Duration::from_millis(120)),
            rules: vec![rule(
                "r1",
                &[InhibitorKind::AudioPlayback, InhibitorKind::Call],
            )],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // Both kinds assert immediately on startup (both running).
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::AudioPlayback && v
            })
            .await
        );
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::Call && v
            })
            .await
        );

        // One failing tick: state must be kept (no publish at all).
        rewrite_script(&script, "#!/bin/sh\nexit 1\n");
        assert_no_msg(&mut ctl_rx, poll * 2, |_, _, _| true).await;

        // Second consecutive failing tick: both kinds reset to false.
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::AudioPlayback && !v
            })
            .await
        );
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::Call && !v
            })
            .await
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── reset_then_reassert_waits_min_active (P9) ────────────────────────────

    #[tokio::test]
    async fn reset_then_reassert_waits_min_active() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "pw-dump", &cat_script(MOVIE));
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(50);
        let min_active = Duration::from_millis(300);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, min_active),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // Startup: immediate assert (startup_grace consumed here).
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::AudioPlayback && v
            })
            .await
        );

        // Two failures reset to inactive.
        rewrite_script(&script, "#!/bin/sh\nexit 1\n");
        assert!(
            wait_for_msg(&mut ctl_rx, Duration::from_secs(1), |_, k, v| {
                k == InhibitorKind::AudioPlayback && !v
            })
            .await
        );

        let reset_at = tokio::time::Instant::now();
        // Stream still running — but startup_grace is already spent, so the
        // reassertion must wait a FRESH min_active, not fire immediately.
        rewrite_script(&script, &cat_script(MOVIE));

        assert_no_msg(&mut ctl_rx, min_active.mul_f32(0.6), |_, k, v| {
            k == InhibitorKind::AudioPlayback && v
        })
        .await;

        let reasserted = wait_for_msg(&mut ctl_rx, Duration::from_secs(2), |_, k, v| {
            k == InhibitorKind::AudioPlayback && v
        })
        .await;
        assert!(reasserted, "must reassert once a fresh min_active elapses");
        assert!(
            reset_at.elapsed() >= min_active,
            "reassert must not beat a fresh min_active window"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── breaker_trips_and_recovery_asserts_immediately ───────────────────────

    #[tokio::test]
    async fn breaker_trips_and_recovery_asserts_immediately() {
        let dir = tempfile::tempdir().unwrap();
        // A script that runs long enough to outlive the fill phase, then
        // exits on its own — no genuinely SIGKILL-immune process is needed
        // (R3-M/C): the scripted probe below never kills it, so it simply
        // keeps running (the test seam, not OS trickery) until it exits.
        let script = write_script(dir.path(), "pw-dump", "#!/bin/sh\nsleep 3.0\n");
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(20);
        // min_active is set far larger than the breaker-recovery tick's own
        // overhead so ordinary debounce (a fresh min_active wait since
        // active_since) could never land inside the tight deadline below --
        // only startup_grace, untouched by the breaker episode, can.
        let min_active = Duration::from_secs(4);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, min_active),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, always_unreapable_probe(), cancel.clone()));

        // Give the fill phase generous real time to push all 8 unreapable
        // ticks (empirically ~100ms/tick including spawn overhead) well
        // before any of the fake children's own 3s sleep elapses.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Swap to the movie fixture while the breaker is tripped — the swap
        // only takes effect once spawning resumes.
        let swap_at = tokio::time::Instant::now();
        rewrite_script(&script, &cat_script(MOVIE));

        // Recovery: the 8 fake children (3s sleep) exit on their own; once
        // the reap list drains below cap, spawning resumes and the movie
        // fixture must assert IMMEDIATELY (startup_grace survived both the
        // failures and the breaker episode) -- well inside this tight
        // deadline, whereas ordinary debounce would need an extra
        // min_active (4s) on top of the recovery tick and blow it.
        let asserted = wait_for_msg(&mut ctl_rx, Duration::from_millis(3500), |_, k, v| {
            k == InhibitorKind::AudioPlayback && v
        })
        .await;
        assert!(
            asserted,
            "post-breaker recovery with a running stream must assert immediately"
        );
        assert!(
            swap_at.elapsed() < min_active,
            "assert must land well before a full min_active window elapses"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── no_rules_spawns_nothing ───────────────────────────────────────────────

    #[tokio::test]
    async fn no_rules_spawns_nothing() {
        let (ctl, _ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let result = crate::inhibit_audio::spawn(vec![], AudioConfig::default(), ctl, cancel);
        assert!(result.is_none(), "no audio rules must spawn nothing");
    }

    // ── tick_timeout_skips_and_counts_as_failure (P1 — REAL kill path) ───────

    #[tokio::test]
    async fn tick_timeout_skips_and_counts_as_failure() {
        let dir = tempfile::tempdir().unwrap();
        // Sleep-forever script: genuinely killable (not the seam) — pins the
        // real start_kill + bounded-wait path.
        let script = write_script(dir.path(), "pw-dump", "#!/bin/sh\nexec sleep 100000\n");
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(80);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, Duration::from_millis(100)),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        // REAL production probe — the seam is deliberately NOT used here.
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // Two consecutive timeouts must reset to inactive (first publish
        // ever, since nothing was ever asserted — the sleep-forever script
        // never produces a successful classification).
        let reset = wait_for_msg(&mut ctl_rx, Duration::from_secs(2), |_, k, v| {
            k == InhibitorKind::AudioPlayback && !v
        })
        .await;
        assert!(
            reset,
            "two consecutive tick timeouts must count as failures and publish inactive"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── poller_caps_stdout_at_4mib (P2 — real pipe read) ─────────────────────

    #[tokio::test]
    async fn poller_caps_stdout_at_4mib() {
        let dir = tempfile::tempdir().unwrap();
        // Emits > 4 MiB without ever producing valid JSON or exiting
        // promptly — the bounded read must engage well before the
        // poll_interval timeout would otherwise fire.
        let script = write_script(
            dir.path(),
            "pw-dump",
            "#!/bin/sh\nyes 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' | head -c 5000000\n",
        );
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        // poll_interval is deliberately generous (1s): two CONSECUTIVE
        // failures are needed to observe the reset publish (F11), and each
        // tick is followed by a poll_interval sleep regardless — so the
        // floor for two failures is ~1 poll_interval no matter how fast the
        // cap engages. What this pins is that the cap resolves each tick
        // almost instantly rather than by waiting out the FULL 1s timeout
        // per tick (which would push total elapsed past ~2s).
        let poll = Duration::from_secs(1);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(&script, poll, Duration::from_millis(100)),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let start = tokio::time::Instant::now();
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        let reset = wait_for_msg(&mut ctl_rx, Duration::from_secs(4), |_, k, v| {
            k == InhibitorKind::AudioPlayback && !v
        })
        .await;
        assert!(
            reset,
            "oversized stdout must resolve through the cap-exceeded failure path"
        );
        // Two failing ticks + one inter-tick sleep_or_cancel(poll_interval)
        // should total roughly ~1 poll_interval if the cap fast-path
        // engaged both times; if it fell through to the real per-tick
        // timeout instead, total would approach ~3 poll_intervals (two
        // timed-out ticks + the sleep between them).
        assert!(
            start.elapsed() < poll.mul_f32(1.8),
            "the read cap must engage without waiting out poll_interval; took {:?}",
            start.elapsed()
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // ── spawn_failure_missing_binary (P10) ────────────────────────────────────

    #[tokio::test]
    async fn spawn_failure_missing_binary() {
        let (ctl, mut ctl_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let poll = Duration::from_millis(40);
        let deps = AudioDeps {
            ctl,
            cfg: cfg(
                &PathBuf::from("/nonexistent/definitely-not-pw-dump"),
                poll,
                Duration::from_millis(100),
            ),
            rules: vec![rule("r1", &[InhibitorKind::AudioPlayback])],
        };
        let handle = tokio::spawn(run_loop(deps, production_reap_probe(), cancel.clone()));

        // A missing binary counts as a failure like any other (two
        // consecutive → publish inactive), distinct only in its log event.
        let reset = wait_for_msg(&mut ctl_rx, Duration::from_secs(2), |_, k, v| {
            k == InhibitorKind::AudioPlayback && !v
        })
        .await;
        assert!(
            reset,
            "a missing pw_dump_command binary must count as a failure"
        );

        cancel.cancel();
        let _ = handle.await;
    }
}
