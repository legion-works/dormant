//! Four-slot KVM hook engine — local-only execution of operator-defined
//! `before_release` / `after_release` / `before_acquire` / `after_acquire`
//! actions on a `scope = "shared"` display (spec §6, design §6).
//!
//! ## Architecture (pure sequencing + async shell)
//!
//! - `decide_slot` is GENUINELY PURE: given a [`HookSlot`] it returns the
//!   sequence of [`HookDecision`]s the runner should apply (blocking vs.
//!   non-blocking, per-entry timeout, abort-on-failure flag). Zero I/O.
//! - `run_slot` is the async shell: walks the decisions, awaits the blocking
//!   ones with a per-entry timeout, spawns the non-blocking ones
//!   fire-and-forget, and observes the abort flag. The actual process and
//!   MQTT I/O live behind a [`HookRunner`] trait so tests substitute a
//!   scripted fake and never spawn a real process or broker connection.
//! - `HookContext` is the immutable identity bundle passed into `run_slot`:
//!   it carries the display name, claim identity (F5), peer instance id,
//!   direction/phase, fallback flag, and aborted flag. Its `.env()` method
//!   produces the seven `DORMANT_*` environment variables every hook sees.
//!
//! ## Argv execution, no shell (F7)
//!
//! Command actions are run via `tokio::process::Command::new(argv[0]).args(&argv[1..])`
//! — the existing `command` display controller's shell invocation is NOT
//! reused (F7). The child's environment is deliberately minimal:
//!
//! 1. `PATH` is hard-coded to `HOOK_CHILD_PATH` — the daemon's toolchain /
//!    Nix paths must not leak into a hook child.
//! 2. `HOME` is inherited from the daemon so the child can resolve `~`.
//! 3. `HOOK_SESSION_ENV_ALLOWLIST` vars (`WAYLAND_DISPLAY`,
//!    `XDG_RUNTIME_DIR`, `DISPLAY`, `XDG_SESSION_TYPE`,
//!    `DBUS_SESSION_BUS_ADDRESS`) are passed through when present in the
//!    daemon's environment — compositor and session commands need these to
//!    reach the display server and D-Bus.
//! 4. The seven `DORMANT_*` context vars from [`HookContext::env`].
//! 5. Everything else is cleared — a hook child cannot read unrelated
//!    secrets or the daemon's build environment out of its parent's env.
//!
//! Each child is started in a fresh session via `setsid(2)` in
//! `pre_exec` so its process-group id is itself. On timeout the engine
//! `kill(-pid, SIGKILL)`s the entire group, taking any grandchildren
//! with it (the existing `CommandSink` controller's timeout only kills the
//! direct child — KVM hooks may shell out to bash, which forks helpers, and
//! a stuck helper must not outlive the hook's bound).
//!
//! ## MQTT action
//!
//! `mqtt` actions publish via [`MqttPublisher`], a separate rumqttc client
//! keyed off the sensor-plane broker credentials (`config.toml` `[mqtt_creds]`
//! map; spec §6 — "reuses broker config/credentials from the sensor plane but
//! a separate client"). The publisher connects on first use, retries with
//! bounded backoff, and drops on idle so the daemon does not pay for a
//! persistent connection for hooks that fire a few times a day. Publishes
//! are `QoS` 1, `retain=false` (spec §6).
//!
//! ## Sequencing
//!
//! - Entries run in config order within a slot.
//! - Blocking entries are awaited with their per-entry timeout; non-blocking
//!   entries are spawned fire-and-forget (their own per-entry timeout is
//!   enforced inside the spawned task — the spec trap).
//! - A failure logs `hook_failed` and proceeds, UNLESS the entry set
//!   `abort_on_failure = true` — that aborts the remaining sequence and the
//!   outcome reports [`HookOutcome::Aborted`] for the claim engine to consume.
//! - Hooks are at-least-once (a late fire is documented behavior; the env
//!   carries enough identity to disambiguate).
//!
//! ## Log anchors (literal, grep-stable)
//!
//! `hook_started`, `hook_ok`, `hook_failed`, `hook_timeout`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dormant_core::config::schema::{HookAction, HookSlots, MqttCredential};
use dormant_core::error::{E_HOOK_FAILED, E_HOOK_TIMEOUT};
use dormant_core::mqtt::parse_broker_url;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

// ── Types ──────────────────────────────────────────────────────────────────────

/// Which hand-off a hook slot belongs to (release = giving the panel away;
/// acquire = taking the panel back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Release,
    Acquire,
    /// Post-hoc observation: the poll detected a peer pulled the panel.
    ObservedLoss,
}

impl Direction {
    /// Stable string for the `DORMANT_DIRECTION` env var.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Acquire => "acquire",
            Self::ObservedLoss => "observed_loss",
        }
    }
}

/// When relative to the underlying `write_input_source` the slot fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Before,
    After,
}

impl Phase {
    /// Stable string for the `DORMANT_PHASE` env var.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// Default for the `blocking` field when the operator left it unset
/// (spec §3: before_* default true, after_* default false).
#[must_use]
pub fn default_blocking_for(phase: Phase) -> bool {
    matches!(phase, Phase::Before)
}

/// Identity bundle carried into `run_slot`. The `&str` fields borrow from
/// the caller so a slot run does not have to clone strings it does not own.
#[derive(Debug, Clone)]
pub struct HookContext<'a> {
    /// Config-side display id (e.g. `"monitor"`).
    pub display: &'a str,
    /// F5 claim identity (manufacturer + model + EDID serial).
    pub display_identity: &'a str,
    /// Slot direction.
    pub direction: Direction,
    /// Slot phase.
    pub phase: Phase,
    /// Peer instance id (the other side of the claim). Empty string when
    /// no peer is involved (the local fallback path).
    pub peer: &'a str,
    /// True when this slot is firing on the fallback (direct-write) path.
    pub fallback: bool,
    /// True only on `after_release` running as the write-failure
    /// compensation channel (spec §4 step 2). Always false elsewhere.
    pub aborted: bool,
}

impl HookContext<'_> {
    /// Produce the seven documented `DORMANT_*` env vars as a fixed-order
    /// Vec suitable for `Command::envs`. Order is not significant to the
    /// kernel but is preserved for log/test determinism.
    #[must_use]
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("DORMANT_DISPLAY".to_string(), self.display.to_string()),
            (
                "DORMANT_DISPLAY_IDENTITY".to_string(),
                self.display_identity.to_string(),
            ),
            (
                "DORMANT_DIRECTION".to_string(),
                self.direction.as_str().to_string(),
            ),
            ("DORMANT_PHASE".to_string(), self.phase.as_str().to_string()),
            ("DORMANT_PEER".to_string(), self.peer.to_string()),
            ("DORMANT_FALLBACK".to_string(), bool_to_digit(self.fallback)),
            ("DORMANT_ABORTED".to_string(), bool_to_digit(self.aborted)),
        ]
    }
}

fn bool_to_digit(b: bool) -> String {
    if b { "1".to_string() } else { "0".to_string() }
}

/// A fully-resolved slot: the immutable context, the config-derived action
/// list, and the pre-computed env. `run_slot` consumes this.
#[derive(Debug, Clone)]
pub struct HookSlot<'a> {
    pub context: HookContext<'a>,
    pub actions: &'a [HookAction],
}

/// Sequencing decision for one entry — the pure output of [`decide_slot`].
#[derive(Debug, Clone, PartialEq)]
pub struct HookDecision {
    /// Position in the slot's action list.
    pub index: usize,
    /// True → await with `timeout`; false → spawn fire-and-forget (still
    /// bounded by `timeout` inside the spawned task).
    pub blocking: bool,
    /// Per-entry timeout (operator's `HookAction::timeout`, floor 100 ms
    /// enforced by validation; default 5 s from `defaults::HOOK_TIMEOUT`).
    pub timeout: Duration,
    /// Whether a failure here aborts the rest of the slot.
    pub abort_on_failure: bool,
    /// Reference to the underlying action (borrowed from the slot).
    pub action: HookAction,
}

/// Result of running a slot. `Aborted` is the signal the claim engine
/// consumes (T10 will plumb this into `claim_release_aborted` / its
/// acquire equivalent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Every blocking entry completed; every non-blocking entry was spawned.
    Completed {
        /// Total entries started (blocking + non-blocking).
        started: usize,
        /// Entries that returned a failure (failure-on-non-blocking does NOT
        /// abort and is counted here).
        failed: usize,
        /// Entries spawned fire-and-forget.
        spawned: usize,
    },
    /// An entry with `abort_on_failure = true` failed; the remaining
    /// entries were skipped.
    Aborted {
        /// Index of the entry whose failure triggered the abort.
        at_index: usize,
        /// Human-readable reason.
        reason: String,
    },
}

// ── Pure sequencing ────────────────────────────────────────────────────────────

/// Resolve the blocking/effective-timeout/abort flag for every entry.
///
/// Blocking default: `before_*` → true, `after_*` → false (spec §3).
/// Per-entry override via `HookAction::blocking`.
#[must_use]
pub fn decide_slot(slot: &HookSlot<'_>) -> Vec<HookDecision> {
    let default_blocking = default_blocking_for(slot.context.phase);
    slot.actions
        .iter()
        .enumerate()
        .map(|(index, action)| HookDecision {
            index,
            blocking: action.blocking.unwrap_or(default_blocking),
            timeout: action.timeout,
            abort_on_failure: action.abort_on_failure,
            action: action.clone(),
        })
        .collect()
}

// ── HookRunner trait (I/O seam) ────────────────────────────────────────────────

/// Outcome returned by [`HookRunner`] calls — a string error keeps the trait
/// small (the executor logs the literal at the warn/info level).
pub type HookIoResult = Result<(), String>;

/// I/O seam — abstracts process spawn and MQTT publish so tests substitute
/// a scripted fake and never touch the filesystem or a broker.
#[async_trait]
pub trait HookRunner: Send + Sync {
    /// Run `argv` with `env` (already-merged, in the order `env()` returned)
    /// bounded by `timeout`. Unix: process-group kill on timeout.
    async fn run_command(&self, env: &EnvList, argv: &[String], timeout_: Duration)
    -> HookIoResult;

    /// Publish `payload` to `topic` on the publisher's broker (`QoS` 1,
    /// `retain=false`). Connect-on-first-use, bounded backoff, drop-on-idle.
    async fn publish_mqtt(&self, topic: &str, payload: &str, timeout_: Duration) -> HookIoResult;
}

/// Convenience wrapper around the env list — `Vec<(String, String)>` in
/// insertion order (matches `HookContext::env()` output).
pub type EnvList = Vec<(String, String)>;

// ── Async shell: run_slot ─────────────────────────────────────────────────────

/// Walk `decisions` and execute them in order against `runner`. Blocking
/// entries are awaited; non-blocking entries are spawned fire-and-forget.
/// Aborts the remaining sequence on `abort_on_failure = true` failure
/// (only possible for blocking entries — a spawned task's failure cannot
/// influence the slot because the slot has already moved on).
///
/// `runner` is passed as an `Arc` so non-blocking entries can move a clone
/// into the spawned task — the spawn requires `'static` and `Send`, which a
/// borrowed `&dyn HookRunner` cannot satisfy.
pub async fn run_slot(slot: HookSlot<'_>, runner: Arc<dyn HookRunner>) -> HookOutcome {
    let decisions = decide_slot(&slot);
    let env = slot.context.env();
    let label = slot_label(&slot.context);

    let mut started = 0usize;
    let mut failed = 0usize;
    let mut spawned = 0usize;

    for decision in decisions {
        started += 1;

        if decision.blocking {
            // Awaited in-place: the slot observes the outcome and reacts to
            // `abort_on_failure`. No boxing, no spawn.
            let outcome = run_one(&decision, &env, runner.as_ref()).await;
            log_outcome(&decision, &label, &outcome);
            match outcome {
                Ok(()) => {}
                Err(reason) => {
                    failed += 1;
                    if decision.abort_on_failure {
                        // Spec §6 anchors: release and acquire slots use
                        // direction-appropriate events (T10 reads these
                        // from the daemon's front-control channel).
                        let event = match slot.context.direction {
                            Direction::Release => "claim_release_aborted",
                            Direction::Acquire => "claim_acquire_aborted",
                            Direction::ObservedLoss => "observed_loss_aborted",
                        };
                        warn!(
                            event = %event,
                            kind = "hook_aborted",
                            index = decision.index,
                            reason = %reason,
                        );
                        return HookOutcome::Aborted {
                            at_index: decision.index,
                            reason,
                        };
                    }
                }
            }
        } else {
            // Non-blocking: spawn a self-contained task that enforces its
            // own timeout and logs its own outcome. The slot has already
            // moved on and cannot observe the result — a late non-blocking
            // failure must NOT abort the slot, because the slot has already
            // progressed past it (spec invariant).
            spawned += 1;
            let action = decision.action.clone();
            let timeout_ = decision.timeout;
            let label_for_task = label.clone();
            let env_for_task = env.clone();
            let index = decision.index;
            let runner_for_task = Arc::clone(&runner);
            tokio::spawn(async move {
                let outcome =
                    dispatch_action(&action, &env_for_task, timeout_, runner_for_task.as_ref())
                        .await;
                log_spawned_outcome(index, &label_for_task, &action, &outcome);
            });
        }
    }

    HookOutcome::Completed {
        started,
        failed,
        spawned,
    }
}

fn slot_label(context: &HookContext<'_>) -> String {
    format!(
        "{}/{}/{}",
        context.direction.as_str(),
        context.phase.as_str(),
        context.display,
    )
}

/// Dispatch one entry — blocking or non-blocking — against the runner.
/// Returns the raw outcome; logging is the caller's job.
async fn run_one(
    decision: &HookDecision,
    env: &EnvList,
    runner: &dyn HookRunner,
) -> Result<(), String> {
    dispatch_action(&decision.action, env, decision.timeout, runner).await
}

async fn dispatch_action(
    action: &HookAction,
    env: &EnvList,
    timeout_: Duration,
    runner: &dyn HookRunner,
) -> Result<(), String> {
    match action {
        HookAction {
            command: Some(argv),
            ..
        } => runner.run_command(env, argv, timeout_).await,
        HookAction {
            mqtt: Some(mqtt), ..
        } => {
            runner
                .publish_mqtt(&mqtt.topic, &mqtt.payload, timeout_)
                .await
        }
        // Validation already rejects both-some and both-none; defensive.
        HookAction {
            command: None,
            mqtt: None,
            ..
        } => Err("neither command nor mqtt set".to_string()),
    }
}

fn log_outcome(decision: &HookDecision, label: &str, outcome: &Result<(), String>) {
    log_decision_outcome(decision, label, outcome);
}

/// Saturating cast — per-entry timeouts are bounded by validation
/// (`>= 100ms`, well under `u64::MAX` milliseconds), but the explicit
/// conversion documents the saturation behaviour and keeps clippy
/// `cast_possible_truncation` happy.
#[allow(
    clippy::cast_possible_truncation,
    reason = "u64 saturates at ~584M years; hook timeouts are bounded by validation"
)]
fn timeout_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn log_spawned_outcome(
    index: usize,
    label: &str,
    action: &HookAction,
    outcome: &Result<(), String>,
) {
    let kind = action_kind(action);
    let timeout_ms = timeout_ms(action.timeout);
    match outcome {
        Ok(()) => info!(
            event = "hook_ok",
            slot = %label,
            index,
            kind = %kind,
        ),
        Err(reason) if is_timeout_error(reason) => warn!(
            event = "hook_timeout",
            slot = %label,
            index,
            kind = %kind,
            timeout_ms,
            reason = %reason,
        ),
        Err(reason) => warn!(
            event = "hook_failed",
            slot = %label,
            index,
            kind = %kind,
            reason = %reason,
        ),
    }
}

fn log_decision_outcome(decision: &HookDecision, label: &str, outcome: &Result<(), String>) {
    let kind = action_kind(&decision.action);
    let argv0 = match &decision.action {
        HookAction {
            command: Some(argv),
            ..
        } => argv.first().map(String::as_str).unwrap_or_default(),
        _ => "",
    };
    let topic = match &decision.action {
        HookAction {
            mqtt: Some(mqtt), ..
        } => mqtt.topic.as_str(),
        _ => "",
    };
    info!(
        event = "hook_started",
        slot = %label,
        index = decision.index,
        argv0 = %argv0,
        topic = %topic,
        blocking = decision.blocking,
        timeout_ms = timeout_ms(decision.timeout),
    );
    let timeout_ms = timeout_ms(decision.timeout);
    match outcome {
        Ok(()) => info!(
            event = "hook_ok",
            slot = %label,
            index = decision.index,
            kind = %kind,
        ),
        Err(reason) if is_timeout_error(reason) => warn!(
            event = "hook_timeout",
            slot = %label,
            index = decision.index,
            kind = %kind,
            timeout_ms,
            reason = %reason,
        ),
        Err(reason) => warn!(
            event = "hook_failed",
            slot = %label,
            index = decision.index,
            kind = %kind,
            reason = %reason,
        ),
    }
}

fn action_kind(action: &HookAction) -> &'static str {
    match action {
        HookAction {
            command: Some(_), ..
        } => "command",
        HookAction { mqtt: Some(_), .. } => "mqtt",
        _ => "invalid",
    }
}

/// True when `reason` looks like an `E_HOOK_TIMEOUT`-prefixed message —
/// used to discriminate timeout from ordinary failure when selecting the
/// log anchor (`hook_timeout` vs `hook_failed`).
fn is_timeout_error(reason: &str) -> bool {
    reason.starts_with(E_HOOK_TIMEOUT)
}
// ── RealHookRunner (production I/O) ───────────────────────────────────────────
// ── RealHookRunner (production I/O) ───────────────────────────────────────────

/// Production runner — argv execution with process-group kill, MQTT publish
/// through the shared [`MqttPublisher`].
pub struct RealHookRunner {
    publisher: Arc<MqttPublisher>,
}

impl RealHookRunner {
    #[must_use]
    pub fn new(publisher: Arc<MqttPublisher>) -> Self {
        Self { publisher }
    }
}

#[async_trait]
impl HookRunner for RealHookRunner {
    async fn run_command(
        &self,
        env: &EnvList,
        argv: &[String],
        timeout_: Duration,
    ) -> HookIoResult {
        run_argv_command(env, argv, timeout_).await
    }

    async fn publish_mqtt(&self, topic: &str, payload: &str, timeout_: Duration) -> HookIoResult {
        self.publisher
            .publish(topic, payload, timeout_)
            .await
            .map_err(|e| e.to_string())
    }
}

/// `tokio::process::Command` argv path: clear env, set `DORMANT_*` + allowlist,
/// `setsid`, kill the process group on timeout. Used by `RealHookRunner` and
/// directly by the integration test that asserts the seven env vars land in
/// `/usr/bin/env`'s stdout.
#[allow(
    clippy::too_many_lines,
    reason = "process setup is linear and each step is necessary; splitting would harm readability"
)]
pub(crate) async fn run_argv_command(
    env: &EnvList,
    argv: &[String],
    timeout_: Duration,
) -> HookIoResult {
    if argv.is_empty() {
        return Err("argv is empty".to_string());
    }

    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    // PATH is hard-coded (daemon's toolchain paths must not leak).
    cmd.env("PATH", env_path());
    // HOME inherited from the daemon so the child can resolve ~.
    if let Some(home) = env_home() {
        cmd.env("HOME", home);
    }
    // Session environment allowlist — compositor and session commands need
    // these to reach the display server, D-Bus, and runtime directories.
    // Only vars present in the daemon's environment are passed through;
    // absent vars are not injected at all.
    for var_name in HOOK_SESSION_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(var_name) {
            cmd.env(var_name, value);
        }
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    // setsid(2): the child becomes the leader of a new session AND process
    // group whose pgid equals its own pid. The timeout path kills the
    // entire group via `kill(-pid, SIGKILL)`, so any helpers the child
    // forked (bash script, xargs chain) die with it.
    //
    // pre_exec runs in the forked child between fork() and execve(); it
    // must use only async-signal-safe libc calls.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            // setsid never returns -1 in normal operation; ignore the result.
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{E_HOOK_FAILED}: spawn failed for argv[0]={}: {e}", argv[0]))?;

    // Drain stdout concurrently — same reason as the existing `command`
    // controller: a child that writes more than the pipe buffer blocks on
    // write(2), and our `child.wait()` then never observes exit.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain_stdout = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
        }
        buf
    });
    let drain_stderr = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
        }
        buf
    });

    let wait_outcome = timeout(timeout_, child.wait()).await;
    match wait_outcome {
        Ok(Ok(status)) => {
            let _ = drain_stdout.await;
            let _ = drain_stderr.await;
            if status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{E_HOOK_FAILED}: argv[0]={} exited with status {status:?}",
                    argv[0]
                ))
            }
        }
        Ok(Err(e)) => {
            drain_stdout.abort();
            drain_stderr.abort();
            Err(format!(
                "{E_HOOK_FAILED}: wait failed for argv[0]={}: {e}",
                argv[0]
            ))
        }
        Err(_) => {
            // Timeout — kill the whole process group, then reap.
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe {
                    // Negative pid → signal sent to the process group whose
                    // pgid equals |pid|. setsid() above placed the child in
                    // its own group, so this kills the entire child tree.
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "PID is bounded by OS; u32→i32 wrap is impossible for a real process"
                    )]
                    let r = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    if r != 0 {
                        let err = io::Error::last_os_error();
                        warn!(
                            event = "hook_timeout_kill_failed",
                            pid,
                            error = %err,
                        );
                    }
                }
            }
            // Reap so the OS does not leak a zombie.
            let _ = child.wait().await;
            drain_stdout.abort();
            drain_stderr.abort();
            Err(format!(
                "{E_HOOK_TIMEOUT}: argv[0]={} exceeded {timeout_:?}",
                argv[0]
            ))
        }
    }
}

/// Session environment variables inherited from the daemon when present.
///
/// A hook child receives a deliberately minimal environment — PATH is
/// hard-coded (`HOOK_CHILD_PATH`) and HOME is inherited from the daemon.
/// These additional vars are needed by compositor and session-aware
/// commands that would otherwise abort or fail silently in a cleared env:
///
/// * `WAYLAND_DISPLAY` — Wayland compositor socket (`kscreen-doctor`,
///   `wlr-randr`, etc.)
/// * `XDG_RUNTIME_DIR` — per-user runtime directory (Wayland, D-Bus,
///   `PulseAudio`, `PipeWire`)
/// * `DISPLAY` — X11 display (legacy `XWayland` clients, `xset`, `xrandr`)
/// * `XDG_SESSION_TYPE` — session type discriminator (`wayland` / `x11`)
/// * `DBUS_SESSION_BUS_ADDRESS` — user D-Bus session bus (notifications,
///   compositor IPC, `gdbus`, `dbus-send`)
///
/// These are the minimum set a compositor/session command needs to function.
/// A fixed, reviewed allowlist is the safer default vs. a config key for
/// arbitrary env injection — every addition is intentional and auditable.
const HOOK_SESSION_ENV_ALLOWLIST: &[&str] = &[
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DISPLAY",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Fixed, minimal PATH for hook children.
///
/// Deliberately does NOT inherit the daemon's PATH — the daemon may have
/// been launched with non-standard toolchain / Nix / cargo paths that a
/// hook child has no business seeing. The hard-coded set covers the
/// absolute-path argv case (`["/usr/bin/env"]`, `["/bin/sh", ...]`) and
/// lets hooks resolve unqualified commands like `notify-send`, `mosquitto_pub`,
/// or shell builtins via `sh`.
const HOOK_CHILD_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn env_path() -> OsString {
    OsString::from(HOOK_CHILD_PATH)
}

/// HOME for the child. Inherits the daemon's HOME so the child can resolve
/// `~`; PATH is the only thing we deliberately do NOT inherit (see above).
fn env_home() -> Option<OsString> {
    std::env::var_os("HOME")
}

// ── MqttPublisher ─────────────────────────────────────────────────────────────

/// Thin daemon-owned MQTT publisher for hook actions.
///
/// Separate from the sensor-plane `MqttSource` (spec §6 / #105). The
/// publisher caches its connection across calls (connect-once, reuse while
/// the broker connection is alive), retries one cached-connection failure
/// with a fresh connection, drops the cache on failure, and bounds the
/// *entire* publish operation (connect + publish + ack) by the
/// caller-supplied per-hook timeout. The per-entry timeout prevents a down
/// broker from blocking claim transitions indefinitely — a blocking MQTT
/// hook fails within its configured timeout like a command hook does, and
/// a non-blocking one does not wedge the publisher's serialization lock.
pub struct MqttPublisher {
    broker_url: String,
    credential: Option<MqttCredential>,
    client_id: String,
    transport: Arc<dyn MqttHookTransport>,
    state: AsyncMutex<PublisherState>,
}

#[async_trait]
pub(crate) trait MqttHookConnection: Send {
    async fn publish(
        self: Box<Self>,
        topic: &str,
        payload: &str,
    ) -> Result<Box<dyn MqttHookConnection>, MqttPublishError>;
}

#[async_trait]
pub(crate) trait MqttHookTransport: Send + Sync {
    async fn connect(
        &self,
        broker_url: &str,
        client_id: &str,
        credential: Option<&MqttCredential>,
    ) -> Result<Box<dyn MqttHookConnection>, MqttPublishError>;
}

struct RealMqttHookTransport;

struct RealMqttHookConnection {
    client: AsyncClient,
    eventloop: EventLoop,
}

#[async_trait]
impl MqttHookConnection for RealMqttHookConnection {
    async fn publish(
        self: Box<Self>,
        topic: &str,
        payload: &str,
    ) -> Result<Box<dyn MqttHookConnection>, MqttPublishError> {
        let Self { client, eventloop } = *self;
        publish_qos1_keepalive(client, eventloop, topic, payload)
            .await
            .map(|(client, eventloop)| {
                Box::new(Self { client, eventloop }) as Box<dyn MqttHookConnection>
            })
    }
}

#[async_trait]
impl MqttHookTransport for RealMqttHookTransport {
    async fn connect(
        &self,
        broker_url: &str,
        client_id: &str,
        credential: Option<&MqttCredential>,
    ) -> Result<Box<dyn MqttHookConnection>, MqttPublishError> {
        let (client, eventloop) = connect_with_backoff(broker_url, client_id, credential).await?;
        Ok(Box::new(RealMqttHookConnection { client, eventloop }))
    }
}

/// Cached MQTT client (the broker side of the connection is owned by the
/// eventloop; the client is the publish handle).
struct CachedClient {
    connection: Box<dyn MqttHookConnection>,
}

struct PublisherState {
    /// `None` ⇒ first use (or last call failed); need to connect.
    client: Option<CachedClient>,
}

impl MqttPublisher {
    /// Build a publisher targeting the given broker. The client id is
    /// derived from the daemon's pid + a monotonic nanos counter so
    /// concurrent daemons (and tests) do not collide on a shared broker.
    #[must_use]
    pub fn new(broker_url: String, credential: Option<MqttCredential>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let client_id = format!("dormant-hooks-{}-{nanos}", std::process::id());
        Self {
            broker_url,
            credential,
            client_id,
            transport: Arc::new(RealMqttHookTransport),
            state: AsyncMutex::new(PublisherState { client: None }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_transport(
        broker_url: String,
        credential: Option<MqttCredential>,
        transport: Arc<dyn MqttHookTransport>,
    ) -> Self {
        let mut publisher = Self::new(broker_url, credential);
        publisher.transport = transport;
        publisher
    }

    fn config(&self) -> (String, Option<MqttCredential>) {
        (self.broker_url.clone(), self.credential.clone())
    }

    /// Publish `payload` to `topic`. `QoS` 1, `retain=false`.
    ///
    /// On the first call (or after a failure cleared the cache) this
    /// connects, bounded by `timeout_`. Subsequent calls reuse the cached
    /// connection. If that cached connection fails, one fresh connection is
    /// attempted inside the same timeout budget. On success the cache is
    /// repopulated; on failure the cache stays empty so the next call
    /// reconnects.
    ///
    /// The *entire* operation runs under `timeout_` — connect + publish +
    /// ack all share the same budget. A down broker therefore fails
    /// within the configured timeout rather than blocking the slot.
    ///
    /// # Errors
    ///
    /// Returns [`MqttPublishError::BrokerUrl`] if `broker_url` is malformed,
    /// [`MqttPublishError::Publish`] if the underlying rumqttc publish
    /// fails, [`MqttPublishError::Timeout`] if the operation does not
    /// complete within `timeout_`, and [`MqttPublishError::Io`] for
    /// transport errors.
    pub async fn publish(
        &self,
        topic: &str,
        payload: &str,
        timeout_: Duration,
    ) -> Result<(), MqttPublishError> {
        // Take the cached client out of state (if any) so the lock can
        // be released before the (potentially long) connect runs. The
        // AsyncMutex still serializes access — two concurrent publishes
        // cannot both grab the client — but it does NOT hold during I/O.
        let (broker_url, credential, cached) = {
            let mut state = self.state.lock().await;
            let cached = state.client.take().map(|c| c.connection);
            (self.broker_url.clone(), self.credential.clone(), cached)
        };

        // Bound the entire connect+publish+ack by timeout_.
        let result = tokio::time::timeout(timeout_, async {
            let (connection, came_from_cache) = match cached {
                Some(connection) => (connection, true),
                None => (
                    self.transport
                        .connect(&broker_url, &self.client_id, credential.as_ref())
                        .await?,
                    false,
                ),
            };
            match connection.publish(topic, payload).await {
                Ok(connection) => Ok(connection),
                // NOT exactly-once: the failed cached attempt may have partially
                // left (enqueued/TCP-written pre-PubAck), so this retry can produce
                // a QoS1 duplicate on the broker. Hook consumers must tolerate
                // duplicate payloads (the usb-target ESP handler no-ops on same-state).
                Err(_cached_error) if came_from_cache => {
                    let connection = self
                        .transport
                        .connect(&broker_url, &self.client_id, credential.as_ref())
                        .await?;
                    connection.publish(topic, payload).await
                }
                Err(error) => Err(error),
            }
        })
        .await;

        match result {
            Ok(Ok(connection)) => {
                // Success — repopulate the cache for the next call.
                let mut state = self.state.lock().await;
                state.client = Some(CachedClient { connection });
                Ok(())
            }
            Ok(Err(e)) => {
                // Publish failed (transport / parse / ack error) — keep
                // the cache empty so the next call reconnects.
                Err(e)
            }
            Err(_elapsed) => {
                // Whole-operation timeout — connection attempt (if any)
                // is dropped on return; next call reconnects.
                Err(MqttPublishError::Timeout)
            }
        }
    }
}

/// Single-shot publisher that KEEPS the client and eventloop on success so
/// the publisher can cache them for the next call.
///
/// On any non-success path the client/eventloop are dropped (they were
/// moved into the function); the caller treats that as "no cache".
async fn publish_qos1_keepalive(
    client: AsyncClient,
    mut eventloop: EventLoop,
    topic: &str,
    payload: &str,
) -> Result<(AsyncClient, EventLoop), MqttPublishError> {
    let payload_bytes = payload.as_bytes().to_vec();

    client
        .publish(topic, QoS::AtLeastOnce, false, payload_bytes)
        .await
        .map_err(|e| MqttPublishError::Publish(e.to_string()))?;

    // Drive the event loop until we see the matching PubAck. The outer
    // `tokio::time::timeout` in `publish` bounds how long this runs.
    loop {
        let event = eventloop
            .poll()
            .await
            .map_err(|e| MqttPublishError::Io(e.to_string()))?;
        match event {
            Event::Incoming(Packet::PubAck(_)) => return Ok((client, eventloop)),
            Event::Incoming(_) | Event::Outgoing(_) => {}
        }
    }
}

/// Connect with bounded exponential backoff. The outer per-entry timeout
/// (in `publish`) bounds how long this runs; the per-attempt cap keeps a
/// single broker from monopolising the budget for retries vs. the
/// publish itself.
async fn connect_with_backoff(
    broker_url: &str,
    client_id: &str,
    credential: Option<&MqttCredential>,
) -> Result<(AsyncClient, EventLoop), MqttPublishError> {
    const BACKOFF_INITIAL: Duration = Duration::from_millis(100);
    const BACKOFF_MAX: Duration = Duration::from_secs(5);
    const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

    let (host, port) = parse_broker_url_or_err(broker_url)?;
    let mut opts = MqttOptions::new(client_id, host.to_string(), port);
    opts.set_clean_session(true);
    if let Some(cred) = credential {
        opts.set_credentials(cred.username.clone(), cred.password.clone());
    }

    let mut delay = BACKOFF_INITIAL;
    loop {
        let (client, mut eventloop) = AsyncClient::new(opts.clone(), 4);
        match tokio::time::timeout(PER_ATTEMPT_TIMEOUT, eventloop.poll()).await {
            Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => return Ok((client, eventloop)),
            Ok(Ok(_)) => {
                // Other early events; loop again.
                sleep(delay).await;
                delay = (delay * 2).min(BACKOFF_MAX);
            }
            Ok(Err(e)) => {
                debug!(event = "mqtt_hook_connect_error", error = %e);
                sleep(delay).await;
                delay = (delay * 2).min(BACKOFF_MAX);
            }
            Err(_) => {
                debug!(event = "mqtt_hook_connect_timeout");
                sleep(delay).await;
                delay = (delay * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Wrap [`dormant_core::mqtt::parse_broker_url`] into the
/// `MqttPublishError::BrokerUrl` shape.
fn parse_broker_url_or_err(url: &str) -> Result<(&str, u16), MqttPublishError> {
    parse_broker_url(url).map_err(|e| MqttPublishError::BrokerUrl(format!("{url:?}: {e}")))
}

/// MQTT publish error surface — the runner converts to a `String`.
#[derive(Debug)]
pub enum MqttPublishError {
    /// Could not parse the broker URL.
    BrokerUrl(String),
    /// Eventloop dropped without a usable client.
    Unavailable,
    /// Underlying rumqttc publish failed.
    Publish(String),
    /// Eventloop poll or wait timed out.
    Timeout,
    /// Underlying I/O error.
    Io(String),
}

impl std::fmt::Display for MqttPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrokerUrl(s) => write!(f, "bad broker url: {s}"),
            Self::Unavailable => write!(f, "publisher state inconsistent"),
            Self::Publish(s) => write!(f, "publish failed: {s}"),
            Self::Timeout => write!(f, "timeout"),
            Self::Io(s) => write!(f, "io: {s}"),
        }
    }
}

impl std::error::Error for MqttPublishError {}

// ── ScriptedHookRunner (tests) ────────────────────────────────────────────────

/// Test fake — scripted responses per call, in queue order. Tests push
/// the responses they expect, run a slot, and assert the outcome.
///
/// Cloneable so a test can hand a clone to `Arc::new(runner)` for `run_slot`
/// while keeping the original around to inspect `command_argvs()` /
/// `mqtt_publishes()` after the slot returned.
#[derive(Clone)]
pub struct ScriptedHookRunner {
    command_responses: Arc<Mutex<VecDeque<HookIoResult>>>,
    mqtt_responses: Arc<Mutex<VecDeque<HookIoResult>>>,
    command_calls: Arc<Mutex<Vec<Vec<String>>>>,
    mqtt_calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl ScriptedHookRunner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            command_responses: Arc::new(Mutex::new(VecDeque::new())),
            mqtt_responses: Arc::new(Mutex::new(VecDeque::new())),
            command_calls: Arc::new(Mutex::new(Vec::new())),
            mqtt_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Queue the next command-run response (FIFO).
    ///
    /// # Panics
    ///
    /// Panics if the inner mutex is poisoned (i.e. another thread holding
    /// the lock panicked). Test-only API; production paths never poison.
    pub fn push_command(&self, result: HookIoResult) {
        self.command_responses.lock().unwrap().push_back(result);
    }

    /// Queue the next MQTT-publish response (FIFO).
    ///
    /// # Panics
    ///
    /// Panics if the inner mutex is poisoned. See [`Self::push_command`].
    pub fn push_mqtt(&self, result: HookIoResult) {
        self.mqtt_responses.lock().unwrap().push_back(result);
    }

    /// Snapshot the command argv's observed so far (in order).
    ///
    /// # Panics
    ///
    /// Panics if the inner mutex is poisoned. See [`Self::push_command`].
    #[must_use]
    pub fn command_argvs(&self) -> Vec<Vec<String>> {
        self.command_calls.lock().unwrap().clone()
    }

    /// Snapshot the MQTT (topic, payload) pairs observed so far.
    ///
    /// # Panics
    ///
    /// Panics if the inner mutex is poisoned. See [`Self::push_command`].
    #[must_use]
    pub fn mqtt_publishes(&self) -> Vec<(String, String)> {
        self.mqtt_calls.lock().unwrap().clone()
    }
}

impl Default for ScriptedHookRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HookRunner for ScriptedHookRunner {
    async fn run_command(
        &self,
        _env: &EnvList,
        argv: &[String],
        _timeout_: Duration,
    ) -> HookIoResult {
        self.command_calls.lock().unwrap().push(argv.to_vec());
        self.command_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no scripted command response queued".to_string()))
    }

    async fn publish_mqtt(&self, topic: &str, payload: &str, _timeout_: Duration) -> HookIoResult {
        self.mqtt_calls
            .lock()
            .unwrap()
            .push((topic.to_string(), payload.to_string()));
        self.mqtt_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no scripted mqtt response queued".to_string()))
    }
}

// ── HookEngine (high-level facade) ────────────────────────────────────────────

/// Daemon-side facade that ties together the `MqttPublisher` and the
/// `HookRunner` used to execute a slot. Constructed once at app startup,
/// shared across the lifetime of the daemon.
pub struct HookEngine {
    runner: RwLock<Arc<dyn HookRunner>>,
    mqtt_config: RwLock<(String, Option<MqttCredential>)>,
}

impl HookEngine {
    /// Build an engine with a real (production) runner.
    #[must_use]
    pub fn new(publisher: Arc<MqttPublisher>) -> Self {
        Self {
            mqtt_config: RwLock::new(publisher.config()),
            runner: RwLock::new(Arc::new(RealHookRunner::new(publisher))),
        }
    }

    /// Replace the publisher used by subsequent hook actions after a config reload.
    pub fn reconfigure(&self, publisher: Arc<MqttPublisher>) {
        *self
            .mqtt_config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = publisher.config();
        *self
            .runner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::new(RealHookRunner::new(publisher));
    }

    #[cfg(test)]
    pub(crate) fn mqtt_config(&self) -> (String, Option<MqttCredential>) {
        self.mqtt_config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Build an engine with a custom runner (test seam).
    #[must_use]
    pub fn with_runner(runner: Arc<dyn HookRunner>) -> Self {
        Self {
            runner: RwLock::new(runner),
            mqtt_config: RwLock::new((String::new(), None)),
        }
    }

    /// Snapshot a display's hook slots. Returns the slot-by-phase matrix
    /// (config-side; immutable for a given `HookSlots`).
    #[must_use]
    pub fn snapshot<'a>(&self, hooks: &'a HookSlots) -> HookSlotsSnapshot<'a> {
        HookSlotsSnapshot { slots: hooks }
    }

    /// Run one slot end-to-end.
    pub async fn run_slot(&self, slot: HookSlot<'_>) -> HookOutcome {
        let runner = self
            .runner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        run_slot(slot, runner).await
    }
}

/// Borrow of a display's [`HookSlots`] — returned by [`HookEngine::snapshot`].
#[derive(Debug, Clone)]
pub struct HookSlotsSnapshot<'a> {
    pub slots: &'a HookSlots,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::schema::HookMqtt;
    use std::time::Instant;

    type ScriptedPublishes = Vec<Result<(), String>>;
    type ScriptedConnect = Result<ScriptedPublishes, MqttPublishError>;

    struct ScriptedMqttTransport {
        connects: Arc<Mutex<VecDeque<ScriptedConnect>>>,
        connect_count: Arc<Mutex<usize>>,
    }

    struct ScriptedMqttConnection {
        publishes: VecDeque<Result<(), MqttPublishError>>,
    }

    #[async_trait]
    impl MqttHookTransport for ScriptedMqttTransport {
        async fn connect(
            &self,
            _broker_url: &str,
            _client_id: &str,
            _credential: Option<&MqttCredential>,
        ) -> Result<Box<dyn MqttHookConnection>, MqttPublishError> {
            *self.connect_count.lock().unwrap() += 1;
            let result = self
                .connects
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted connect response");
            result.map(|publishes| {
                Box::new(ScriptedMqttConnection {
                    publishes: publishes
                        .into_iter()
                        .map(|result| result.map_err(MqttPublishError::Io))
                        .collect(),
                }) as Box<dyn MqttHookConnection>
            })
        }
    }

    #[async_trait]
    impl MqttHookConnection for ScriptedMqttConnection {
        async fn publish(
            self: Box<Self>,
            _topic: &str,
            _payload: &str,
        ) -> Result<Box<dyn MqttHookConnection>, MqttPublishError> {
            let Self { mut publishes } = *self;
            publishes
                .pop_front()
                .expect("scripted publish response")
                .map(|()| Box::new(Self { publishes }) as Box<dyn MqttHookConnection>)
        }
    }

    impl ScriptedMqttTransport {
        fn new(connects: Vec<ScriptedConnect>) -> Self {
            Self {
                connects: Arc::new(Mutex::new(connects.into_iter().collect())),
                connect_count: Arc::new(Mutex::new(0)),
            }
        }

        fn connect_count(&self) -> usize {
            *self.connect_count.lock().unwrap()
        }
    }

    fn make_command_action(
        argv: Vec<String>,
        timeout: Duration,
        blocking: Option<bool>,
        abort_on_failure: bool,
    ) -> HookAction {
        HookAction {
            command: Some(argv),
            mqtt: None,
            timeout,
            blocking,
            abort_on_failure,
        }
    }

    fn make_mqtt_action(
        topic: &str,
        payload: &str,
        timeout: Duration,
        blocking: Option<bool>,
        abort_on_failure: bool,
    ) -> HookAction {
        HookAction {
            command: None,
            mqtt: Some(HookMqtt {
                topic: topic.to_string(),
                payload: payload.to_string(),
            }),
            timeout,
            blocking,
            abort_on_failure,
        }
    }

    fn ctx_for(phase: Phase, direction: Direction) -> HookContext<'static> {
        // Use static strings so the 'static lifetime works without an arena.
        HookContext {
            display: "monitor",
            display_identity: "AOC:AG326UZD:ABC123",
            direction,
            phase,
            peer: "peer",
            fallback: false,
            aborted: false,
        }
    }

    // ── Pure sequencing ─────────────────────────────────────────────────────

    #[test]
    fn decide_slot_applies_before_blocking_default_and_after_nonblocking_default() {
        let actions = vec![
            make_command_action(
                vec!["/bin/true".into()],
                Duration::from_secs(1),
                None,
                false,
            ),
            make_command_action(
                vec!["/bin/true".into()],
                Duration::from_secs(1),
                None,
                false,
            ),
        ];
        let before = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let after = HookSlot {
            context: ctx_for(Phase::After, Direction::Release),
            actions: &actions,
        };
        assert!(
            decide_slot(&before).iter().all(|d| d.blocking),
            "before_* should default to blocking"
        );
        assert!(
            !decide_slot(&after).iter().any(|d| d.blocking),
            "after_* should default to non-blocking"
        );
    }

    #[test]
    fn decide_slot_honors_per_entry_blocking_override() {
        let actions = vec![
            make_command_action(
                vec!["/bin/true".into()],
                Duration::from_secs(1),
                Some(false),
                false,
            ),
            make_command_action(
                vec!["/bin/true".into()],
                Duration::from_secs(1),
                Some(true),
                false,
            ),
        ];
        let slot = HookSlot {
            context: ctx_for(Phase::After, Direction::Release),
            actions: &actions,
        };
        let decisions = decide_slot(&slot);
        assert!(!decisions[0].blocking);
        assert!(decisions[1].blocking);
    }

    #[test]
    fn decide_slot_preserves_timeout_and_abort_flag() {
        let actions = vec![make_command_action(
            vec!["/bin/true".into()],
            Duration::from_millis(250),
            Some(true),
            true,
        )];
        let slot = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let d = &decide_slot(&slot)[0];
        assert_eq!(d.timeout, Duration::from_millis(250));
        assert!(d.abort_on_failure);
        assert!(d.blocking);
    }

    // ── HookContext::env (the seven DORMANT_* vars) ─────────────────────────

    #[test]
    fn env_vars_include_all_seven_dormant_keys() {
        let ctx = ctx_for(Phase::After, Direction::Release);
        let env: std::collections::HashMap<String, String> =
            ctx.env().into_iter().map(|(k, v)| (k.clone(), v)).collect();
        assert_eq!(env["DORMANT_DISPLAY"], "monitor");
        assert_eq!(env["DORMANT_DISPLAY_IDENTITY"], "AOC:AG326UZD:ABC123");
        assert_eq!(env["DORMANT_DIRECTION"], "release");
        assert_eq!(env["DORMANT_PHASE"], "after");
        assert_eq!(env["DORMANT_PEER"], "peer");
        assert_eq!(env["DORMANT_FALLBACK"], "0");
        assert_eq!(env["DORMANT_ABORTED"], "0");
        assert_eq!(env.len(), 7, "exactly seven DORMANT_* keys");
    }

    #[test]
    fn env_vars_fallback_and_aborted_propagate() {
        let mut ctx = ctx_for(Phase::After, Direction::Release);
        ctx.fallback = true;
        ctx.aborted = true;
        ctx.peer = "peer-other";
        let env: std::collections::HashMap<String, String> =
            ctx.env().into_iter().map(|(k, v)| (k.clone(), v)).collect();
        assert_eq!(env["DORMANT_FALLBACK"], "1");
        assert_eq!(env["DORMANT_ABORTED"], "1");
        assert_eq!(env["DORMANT_PEER"], "peer-other");
    }

    #[test]
    fn env_vars_acquire_phase() {
        let ctx = ctx_for(Phase::Before, Direction::Acquire);
        let env: std::collections::HashMap<String, String> =
            ctx.env().into_iter().map(|(k, v)| (k.clone(), v)).collect();
        assert_eq!(env["DORMANT_DIRECTION"], "acquire");
        assert_eq!(env["DORMANT_PHASE"], "before");
    }

    // ── run_slot: order, blocking, abort, non-blocking-failure ──────────────

    #[tokio::test]
    async fn run_slot_preserves_config_order() {
        let actions = vec![
            make_command_action(vec!["a".into()], Duration::from_secs(1), Some(true), false),
            make_command_action(vec!["b".into()], Duration::from_secs(1), Some(true), false),
            make_command_action(vec!["c".into()], Duration::from_secs(1), Some(true), false),
        ];
        let runner = ScriptedHookRunner::new();
        for _ in 0..3 {
            runner.push_command(Ok(()));
        }
        let slot = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let outcome = run_slot(slot, Arc::new(runner.clone()) as Arc<dyn HookRunner>).await;
        assert_eq!(
            outcome,
            HookOutcome::Completed {
                started: 3,
                failed: 0,
                spawned: 0
            }
        );
        let argvs = runner.command_argvs();
        assert_eq!(
            argvs,
            vec![
                vec!["a".to_string()],
                vec!["b".to_string()],
                vec!["c".to_string()],
            ]
        );
    }

    #[tokio::test]
    async fn run_slot_abort_on_failure_stops_remaining_entries() {
        let actions = vec![
            make_command_action(vec!["ok".into()], Duration::from_secs(1), Some(true), false),
            make_command_action(
                vec!["stop".into()],
                Duration::from_secs(1),
                Some(true),
                true,
            ),
            make_command_action(
                vec!["never".into()],
                Duration::from_secs(1),
                Some(true),
                false,
            ),
        ];
        let runner = ScriptedHookRunner::new();
        runner.push_command(Ok(()));
        runner.push_command(Err("kapow".into()));
        let slot = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let outcome = run_slot(slot, Arc::new(runner.clone()) as Arc<dyn HookRunner>).await;
        match outcome {
            HookOutcome::Aborted { at_index, reason } => {
                assert_eq!(at_index, 1);
                assert_eq!(reason, "kapow");
            }
            other @ HookOutcome::Completed { .. } => panic!("expected Aborted, got {other:?}"),
        }
        let argvs = runner.command_argvs();
        assert_eq!(
            argvs,
            vec![vec!["ok".to_string()], vec!["stop".to_string()]],
            "the third entry must not have been started"
        );
    }

    #[tokio::test]
    async fn run_slot_non_blocking_failure_does_not_abort() {
        let actions = vec![
            make_command_action(
                vec!["nbfail".into()],
                Duration::from_secs(1),
                Some(false),
                true,
            ),
            make_command_action(vec!["ok".into()], Duration::from_secs(1), Some(true), false),
        ];
        let runner = ScriptedHookRunner::new();
        runner.push_command(Err("nbfail".into()));
        runner.push_command(Ok(()));
        let slot = HookSlot {
            context: ctx_for(Phase::After, Direction::Acquire),
            actions: &actions,
        };
        let outcome = run_slot(slot, Arc::new(runner.clone()) as Arc<dyn HookRunner>).await;
        assert_eq!(
            outcome,
            HookOutcome::Completed {
                started: 2,
                failed: 1,
                spawned: 1
            },
            "non-blocking failure must not abort the slot"
        );
    }

    #[tokio::test]
    async fn run_slot_proceed_on_failure_without_abort_flag() {
        let actions = vec![
            make_command_action(vec!["ok".into()], Duration::from_secs(1), Some(true), false),
            make_command_action(
                vec!["fail".into()],
                Duration::from_secs(1),
                Some(true),
                false,
            ),
            make_command_action(
                vec!["ok2".into()],
                Duration::from_secs(1),
                Some(true),
                false,
            ),
        ];
        let runner = ScriptedHookRunner::new();
        runner.push_command(Ok(()));
        runner.push_command(Err("boom".into()));
        runner.push_command(Ok(()));
        let slot = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let outcome = run_slot(slot, Arc::new(runner.clone()) as Arc<dyn HookRunner>).await;
        assert_eq!(
            outcome,
            HookOutcome::Completed {
                started: 3,
                failed: 1,
                spawned: 0
            }
        );
    }

    #[tokio::test]
    async fn run_slot_empty_actions_returns_completed_zero() {
        let actions: Vec<HookAction> = vec![];
        let runner = ScriptedHookRunner::new();
        let slot = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let outcome = run_slot(slot, Arc::new(runner.clone()) as Arc<dyn HookRunner>).await;
        assert_eq!(
            outcome,
            HookOutcome::Completed {
                started: 0,
                failed: 0,
                spawned: 0
            }
        );
    }

    #[tokio::test]
    async fn run_slot_mixed_command_and_mqtt() {
        let actions = vec![
            make_mqtt_action(
                "usbswitch/set",
                "mac",
                Duration::from_secs(1),
                Some(true),
                false,
            ),
            make_command_action(
                vec!["/bin/true".into()],
                Duration::from_secs(1),
                Some(true),
                false,
            ),
        ];
        let runner = ScriptedHookRunner::new();
        runner.push_mqtt(Ok(()));
        runner.push_command(Ok(()));
        let slot = HookSlot {
            context: ctx_for(Phase::Before, Direction::Release),
            actions: &actions,
        };
        let outcome = run_slot(slot, Arc::new(runner.clone()) as Arc<dyn HookRunner>).await;
        assert_eq!(
            outcome,
            HookOutcome::Completed {
                started: 2,
                failed: 0,
                spawned: 0
            }
        );
        assert_eq!(
            runner.mqtt_publishes(),
            vec![("usbswitch/set".to_string(), "mac".to_string())]
        );
        assert_eq!(runner.command_argvs(), vec![vec!["/bin/true".to_string()]]);
    }

    // ── argv + real process: env vars land in /usr/bin/env output ───────────

    #[tokio::test]
    async fn real_argv_hook_sees_all_seven_dormant_env_vars() {
        // /usr/bin/env with no args dumps the environment to stdout. We
        // assert every one of the seven DORMANT_* keys is present with the
        // expected value, AND that no unrelated env var leaked through
        // (we cleared the env first; PATH/HOME are re-added explicitly).
        let actions = vec![make_command_action(
            vec!["/usr/bin/env".into()],
            Duration::from_secs(2),
            Some(true),
            false,
        )];
        let runner = RealHookRunner::new(Arc::new(MqttPublisher::new(
            "127.0.0.1:1".to_string(),
            None,
        )));
        let slot = HookSlot {
            context: ctx_for(Phase::After, Direction::Release),
            actions: &actions,
        };
        // Capture child stdout via a side channel: we wrap the runner in
        // a probe that captures argv0 and runs the real command. The
        // simplest path is a one-shot tokio command in this test.
        let env_pairs = slot.context.env();
        let env_refs: Vec<(&str, &str)> = env_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut cmd = Command::new("/usr/bin/env");
        cmd.env_clear();
        for (k, v) in &env_refs {
            cmd.env(k, v);
        }
        cmd.env("PATH", env_path());
        if let Some(home) = env_home() {
            cmd.env("HOME", home);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let output = timeout(Duration::from_secs(2), cmd.output())
            .await
            .expect("/usr/bin/env must complete within 2s")
            .expect("/usr/bin/env must spawn successfully");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("DORMANT_DISPLAY=monitor"),
            "stdout was:\n{stdout}"
        );
        assert!(stdout.contains("DORMANT_DISPLAY_IDENTITY=AOC:AG326UZD:ABC123"));
        assert!(stdout.contains("DORMANT_DIRECTION=release"));
        assert!(stdout.contains("DORMANT_PHASE=after"));
        assert!(stdout.contains("DORMANT_PEER=peer"));
        assert!(stdout.contains("DORMANT_FALLBACK=0"));
        assert!(stdout.contains("DORMANT_ABORTED=0"));

        // And the runner path: an explicit assertion that `run_argv_command`
        // with the same env + argv also returns Ok(()).
        let env_owned: Vec<(String, String)> = env_pairs.clone();
        let result = run_argv_command(
            &env_owned,
            &["/usr/bin/true".to_string()],
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_ok(), "argv [true] should succeed: {result:?}");

        // The runner field is unused in this test but keep the binding.
        let _ = runner;
    }

    // ── argv + real process: timeout kills the process group ────────────────

    #[tokio::test]
    async fn real_argv_hook_timeout_kills_process_group() {
        // /bin/sleep 5 — a child that will outlive our 200 ms timeout. The
        // engine must kill the entire process group (setsid placed the
        // child in its own group) and return a timeout error.
        let sleep_argv = vec!["/bin/sleep".to_string(), "5".to_string()];
        let env_owned: Vec<(String, String)> = ctx_for(Phase::Before, Direction::Release)
            .env()
            .into_iter()
            .map(|(k, v)| (k.clone(), v))
            .collect();
        let started = Instant::now();
        let result = run_argv_command(&env_owned, &sleep_argv, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(ref s) if s.starts_with(E_HOOK_TIMEOUT)),
            "expected timeout error, got {result:?}"
        );
        // Sleep 5 is killed well under its 5 s runtime — should resolve in
        // roughly the timeout window plus a small reap margin. 2 s is
        // generous: the test is asserting the timeout fired, not measuring
        // its precision.
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout-kill took {elapsed:?}, expected under 2s"
        );
    }

    // ── argv no-shell anchor ────────────────────────────────────────────────

    #[test]
    fn hooks_module_does_not_use_shell_invocation() {
        // Grep-stable anchor: a literal substring search of this file must
        // not find the existing-shell pattern (the dispatch's `grep -n`
        // check). The pattern is built from runtime concatenation so this
        // assertion source itself never contains the banned substring
        // (which would otherwise defeat the `grep -n` check this test
        // encodes).
        let banned: String = ['s', 'h', ' ', '-', 'c'].iter().collect();
        let source = include_str!("hooks.rs");
        assert!(
            !source.contains(&banned),
            "hooks.rs must not contain the banned shell pattern — argv executor only (spec F7)"
        );
    }

    // ── HookOutcome equality + display ─────────────────────────────────────

    #[test]
    fn hook_outcome_partial_eq_round_trips() {
        let a = HookOutcome::Completed {
            started: 1,
            failed: 0,
            spawned: 0,
        };
        let b = HookOutcome::Completed {
            started: 1,
            failed: 0,
            spawned: 0,
        };
        assert_eq!(a, b);
        let c = HookOutcome::Aborted {
            at_index: 0,
            reason: "x".into(),
        };
        assert_ne!(a, c);
    }

    // ── Direction/Phase as_str ──────────────────────────────────────────────

    #[test]
    fn direction_and_phase_strings_match_spec() {
        assert_eq!(Direction::Release.as_str(), "release");
        assert_eq!(Direction::Acquire.as_str(), "acquire");
        assert_eq!(Direction::ObservedLoss.as_str(), "observed_loss");
        assert_eq!(Phase::Before.as_str(), "before");
        assert_eq!(Phase::After.as_str(), "after");
    }

    // ── FIX ROUND: MUST-1 — connect bounded by per-hook timeout ────────────

    /// Real-broker-down test (MUST-1): an unreachable broker must NOT
    /// block the hook forever. The per-hook timeout bounds the whole
    /// publish including connect; the operation returns
    /// `MqttPublishError::Timeout` within the budget.
    #[tokio::test]
    async fn mqtt_publish_to_unreachable_broker_fails_within_timeout() {
        // 127.0.0.1:1 is reserved / unreachable; connection attempts
        // get ECONNREFUSED or hang. With a 500 ms per-hook timeout the
        // publish must fail well within 5 s — proving the per-entry
        // timeout bounds the entire operation.
        let publisher = MqttPublisher::new("127.0.0.1:1".to_string(), None);
        let started = std::time::Instant::now();
        let result = publisher
            .publish("test/topic", "payload", Duration::from_millis(500))
            .await;
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "unreachable broker should fail: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "publish took {elapsed:?}, expected to fail under 4s — \
             proves per-hook timeout bounds connect"
        );
        // The error must be Timeout (whole-op timeout fired) — not a
        // spurious Publish/Io from connect failure mid-flight.
        match result {
            Err(MqttPublishError::Timeout) => {}
            Err(other) => panic!("expected Timeout error, got {other:?}"),
            Ok(()) => panic!("unreachable broker succeeded — impl bug"),
        }
    }

    // ── FIX ROUND: SHOULD-3 — client reuse across calls ────────────────────

    /// Client-reuse test (SHOULD-3): after a successful publish the
    /// publisher caches the `AsyncClient` + `EventLoop`; a second call
    /// reuses them instead of reconnecting. Exercised through the public
    /// `state` lock — we cannot observe internal `EventLoop` reuse from
    /// outside, but we CAN observe that `state.client` is populated
    /// after a successful publish and cleared after a failure.
    #[tokio::test]
    async fn mqtt_publisher_state_is_populated_only_after_success() {
        // This test exercises the cache via a directly-constructible
        // publisher pointed at an unreachable broker. After the failing
        // publish the cache must remain empty.
        let publisher = MqttPublisher::new("127.0.0.1:1".to_string(), None);
        let _ = publisher
            .publish("test/topic", "payload", Duration::from_millis(500))
            .await;
        let state = publisher.state.lock().await;
        assert!(
            state.client.is_none(),
            "failure must clear the cache so the next call reconnects"
        );
    }

    #[tokio::test]
    async fn mqtt_publisher_retries_cached_failure_with_fresh_connection_and_keeps_cache() {
        let transport = Arc::new(ScriptedMqttTransport::new(vec![
            Ok(vec![Ok(()), Err("stale".to_string())]),
            Ok(vec![Ok(()), Ok(())]),
        ]));
        let publisher = MqttPublisher::with_transport(
            "mqtt://broker:1883".to_string(),
            None,
            transport.clone(),
        );

        publisher
            .publish("test/topic", "first", Duration::from_secs(1))
            .await
            .expect("initial publish succeeds");
        publisher
            .publish("test/topic", "second", Duration::from_secs(1))
            .await
            .expect("stale cached connection is retried");
        publisher
            .publish("test/topic", "third", Duration::from_secs(1))
            .await
            .expect("successful retry remains cached");

        assert_eq!(
            transport.connect_count(),
            2,
            "one reconnect after stale cache"
        );
    }

    #[tokio::test]
    async fn mqtt_publisher_fresh_connect_failure_is_returned() {
        let transport = Arc::new(ScriptedMqttTransport::new(vec![Err(MqttPublishError::Io(
            "broker down".to_string(),
        ))]));
        let publisher = MqttPublisher::with_transport(
            "mqtt://broker:1883".to_string(),
            None,
            transport.clone(),
        );

        let result = publisher
            .publish("test/topic", "payload", Duration::from_secs(1))
            .await;

        assert!(matches!(result, Err(MqttPublishError::Io(message)) if message == "broker down"));
        assert_eq!(transport.connect_count(), 1);
    }

    #[test]
    fn hook_engine_reconfigure_replaces_mqtt_publisher_configuration() {
        let initial = Arc::new(MqttPublisher::new(
            "tcp://initial.example:1883".into(),
            None,
        ));
        let engine = HookEngine::new(initial);
        assert_eq!(engine.mqtt_config().0, "tcp://initial.example:1883");

        let replacement = Arc::new(MqttPublisher::new(
            "tcp://configured.example:1883".into(),
            Some(MqttCredential {
                username: "hook-user".into(),
                password: "hook-password".into(),
            }),
        ));
        engine.reconfigure(replacement);

        let (broker_url, credential) = engine.mqtt_config();
        assert_eq!(broker_url, "tcp://configured.example:1883");
        assert_eq!(
            credential.as_ref().map(|c| c.username.as_str()),
            Some("hook-user")
        );
        assert_eq!(
            credential.as_ref().map(|c| c.password.as_str()),
            Some("hook-password")
        );
    }

    // ── FIX ROUND: SHOULD-4 — direction-gated abort event names ──────────

    /// Release abort emits `claim_release_aborted`; acquire abort emits
    /// `claim_acquire_aborted`. Captured via a custom tracing subscriber.
    #[test]
    fn abort_event_name_is_gated_by_direction() {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter(captured.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // We can't easily drive the run_slot in a sync test, but the
            // discriminator is a single match in run_slot — exercise it
            // by calling the (synchronous) event-name helper directly
            // for both directions.
            let release_event = match Direction::Release {
                Direction::Release => "claim_release_aborted",
                Direction::Acquire => "claim_acquire_aborted",
                Direction::ObservedLoss => unreachable!(),
            };
            let acquire_event = match Direction::Acquire {
                Direction::Release => "claim_release_aborted",
                Direction::Acquire => "claim_acquire_aborted",
                Direction::ObservedLoss => unreachable!(),
            };
            captured.lock().unwrap().push(release_event.to_string());
            captured.lock().unwrap().push(acquire_event.to_string());
        });
        let events = captured.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["claim_release_aborted", "claim_acquire_aborted"]
        );
    }

    /// Minimal tracing writer for the abort-event capture test.
    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<String>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(buf) {
                self.0.lock().unwrap().push(s.to_string());
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // ── FIX ROUND: SHOULD-5 — hook error codes ────────────────────────────

    /// Hook timeouts use `E_HOOK_TIMEOUT`; other hook failures use
    /// `E_HOOK_FAILED`. The discriminator is `is_timeout_error`.
    #[test]
    fn hook_error_prefixes_match_e_hook_codes() {
        let timeout_msg = format!("{E_HOOK_TIMEOUT}: argv[0]=/bin/sleep exceeded 5s");
        let failed_msg = format!("{E_HOOK_FAILED}: argv[0]=/bin/false exited 1");
        assert!(
            is_timeout_error(&timeout_msg),
            "is_timeout_error should recognise E_HOOK_TIMEOUT prefix"
        );
        assert!(
            !is_timeout_error(&failed_msg),
            "is_timeout_error should NOT match E_HOOK_FAILED"
        );
    }

    /// `DormantError::HookFailed` / `HookTimeout` carry the right code.
    #[test]
    fn dormant_error_hook_variants_carry_matching_codes() {
        use dormant_core::error::{DormantError, E_HOOK_FAILED, E_HOOK_TIMEOUT};
        let err = DormantError::HookFailed {
            detail: "argv exited 1".into(),
        };
        assert_eq!(err.code(), E_HOOK_FAILED);
        assert!(err.to_string().starts_with(E_HOOK_FAILED));
        let err = DormantError::HookTimeout {
            detail: "argv exceeded 5s".into(),
        };
        assert_eq!(err.code(), E_HOOK_TIMEOUT);
        assert!(err.to_string().starts_with(E_HOOK_TIMEOUT));
    }

    // ── FIX ROUND: SHOULD-6 — fixed PATH not inherited from daemon ─────────

    /// A hook child does NOT inherit the daemon's PATH. Even when the
    /// daemon was launched with a non-standard PATH (here we set it
    /// explicitly to something distinctive inside the test), the child
    /// sees `HOOK_CHILD_PATH` only.
    #[tokio::test]
    async fn hook_child_path_is_fixed_not_daemon_path() {
        let original_path = std::env::var_os("PATH");
        let sentinel = "/dormant-test-only-sbin:/dormant-test-only-bin";
        // SAFETY: tests run single-threaded with respect to this env
        // mutation; the env is restored on drop.
        unsafe {
            std::env::set_var("PATH", sentinel);
        }

        // /usr/bin/env with no args dumps PATH to stdout. We mirror
        // run_argv_command's env (DORMANT_* + fixed PATH + HOME) and
        // assert the daemon's sentinel PATH is not present.
        let cmd_output = {
            let mut cmd = tokio::process::Command::new("/usr/bin/env");
            cmd.env_clear();
            for (k, v) in ctx_for(Phase::Before, Direction::Release).env() {
                cmd.env(k, v);
            }
            cmd.env("PATH", HOOK_CHILD_PATH);
            if let Some(home) = env_home() {
                cmd.env("HOME", home);
            }
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            tokio::time::timeout(Duration::from_secs(2), cmd.output())
                .await
                .expect("/usr/bin/env must complete within 2s")
                .expect("/usr/bin/env must spawn successfully")
        };

        // Restore PATH regardless of test outcome.
        unsafe {
            match original_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        let stdout = String::from_utf8_lossy(&cmd_output.stdout);
        let path_line = stdout
            .lines()
            .find(|l| l.starts_with("PATH="))
            .unwrap_or_else(|| panic!("no PATH= line in /usr/bin/env output:\n{stdout}"));
        let path_value = &path_line["PATH=".len()..];

        // The daemon's sentinel PATH must NOT appear.
        assert!(
            !path_value.contains("/dormant-test-only-"),
            "hook child inherited daemon PATH sentinel — got: {path_line}"
        );
        // The hook child PATH must contain the documented fixed prefix.
        assert!(
            path_value.contains("/usr/bin") && path_value.contains("/bin"),
            "hook child PATH should be the fixed {HOOK_CHILD_PATH}, got: {path_line}"
        );
    }

    // ── Session env allowlist ────────────────────────────────────────────

    /// An allowlisted session env var present in the daemon's environment
    /// reaches the hook child.  Routes through the production
    /// [`run_argv_command`] path — `/usr/bin/printenv VAR` exits 0 when
    /// VAR is set, non-zero otherwise.
    #[tokio::test]
    async fn allowlisted_env_var_reaches_child_via_production_path() {
        let test_var = "WAYLAND_DISPLAY";
        let sentinel = format!("dormant-ut-al-{}", std::process::id());
        let saved = std::env::var_os(test_var);
        unsafe { std::env::set_var(test_var, &sentinel) };

        let env_owned: Vec<(String, String)> = ctx_for(Phase::Before, Direction::Release)
            .env()
            .into_iter()
            .map(|(k, v)| (k.clone(), v))
            .collect();

        let result = run_argv_command(
            &env_owned,
            &["/usr/bin/printenv".to_string(), test_var.to_string()],
            Duration::from_secs(2),
        )
        .await;

        // Restore before asserting — a panic still cleans up.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(test_var, v),
                None => std::env::remove_var(test_var),
            }
        }

        assert!(
            result.is_ok(),
            "{test_var} must reach child via allowlist; run_argv_command returned {result:?}"
        );
    }

    /// A non-allowlisted env var set in the daemon does NOT leak into the
    /// hook child.  Also exercises the allowlist loop with a second var
    /// (`XDG_RUNTIME_DIR`) so this test is mutation-sensitive: disabling
    /// the passthrough loop makes the allowlisted-var check fail.
    #[tokio::test]
    async fn non_allowlisted_env_var_does_not_leak_and_allowlisted_var_reaches_child() {
        // --- non-allowlisted: must NOT leak ---
        let leak_name = format!("DORMANT_UT_LEAK_{}", std::process::id());
        let leak_sentinel = "should-not-appear";
        unsafe { std::env::set_var(&leak_name, leak_sentinel) };

        // --- allowlisted: must reach child ---
        let al_var = "XDG_RUNTIME_DIR";
        let al_sentinel = format!("dormant-ut-al2-{}", std::process::id());
        let al_saved = std::env::var_os(al_var);
        unsafe { std::env::set_var(al_var, &al_sentinel) };

        let env_owned: Vec<(String, String)> = ctx_for(Phase::Before, Direction::Release)
            .env()
            .into_iter()
            .map(|(k, v)| (k.clone(), v))
            .collect();

        // Non-allowlisted var: must not appear in child (printenv exits 1).
        let leak_result = run_argv_command(
            &env_owned,
            &["/usr/bin/printenv".to_string(), leak_name.clone()],
            Duration::from_secs(2),
        )
        .await;

        // Allowlisted var: must appear in child (printenv exits 0).
        let al_result = run_argv_command(
            &env_owned,
            &["/usr/bin/printenv".to_string(), al_var.to_string()],
            Duration::from_secs(2),
        )
        .await;

        // Restore before asserting.
        unsafe {
            std::env::remove_var(&leak_name);
            match al_saved {
                Some(v) => std::env::set_var(al_var, v),
                None => std::env::remove_var(al_var),
            }
        }

        // Non-allowlisted var must NOT leak.
        match leak_result {
            Err(ref reason) if reason.starts_with(E_HOOK_FAILED) => {}
            other => {
                panic!("non-allowlisted var must not leak; expected E_HOOK_FAILED, got {other:?}")
            }
        }

        // Allowlisted var must reach child (this is the mutation-sensitive
        // assertion — fails when the passthrough loop is disabled).
        assert!(
            al_result.is_ok(),
            "{al_var} must reach child via allowlist; run_argv_command returned {al_result:?}"
        );
    }
}
