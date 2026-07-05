//! `kwin-dpms` display controller — blanks/wakes displays via `KWin` `kscreen-doctor`.
//!
//! **Audio-unsafe fallback.**  `kscreen-doctor --dpms off` tears down the
//! output's audio sink, killing audio on any audio-carrying output (HDMI/DP).
//! Prefer [`ddcci`](crate::ddcci) (audio-safe) where DDC/CI is available; use
//! `kwin-dpms` only for outputs with **no DDC/CI and no audio**.
//!
//! This controller runs in the **user session** (`systemd --user`, e.g.
//! `plasma-kwin_wayland.service`), not a system service.  `WAYLAND_DISPLAY`
//! must be set for `kscreen-doctor` to reach the compositor.
//!
//! ## Mechanism — the `--dpms`-is-global quirk
//!
//! `kscreen-doctor --dpms <off|on>` is a **global** flag on Wayland.  The
//! positional argument `--dpms off DP-1` is silently accepted but ignored
//! (it blanks **all** outputs, not just `DP-1`).  The only per-output
//! scoping is `--dpms-excluded <connector>` (repeatable).
//!
//! ### Per-output DPMS (the real algorithm)
//!
//! 1. Enumerate all connector names via `kscreen-doctor -o`.
//! 2. Verify the configured target is present (error if not — do not silently
//!    blank nothing).
//! 3. Compute *others* = all connectors except the target.
//! 4. Run `kscreen-doctor --dpms off --dpms-excluded <other1> --dpms-excluded <other2> …`.
//!    On a single-monitor system there are zero others → no exclusions needed.
//!    On a multi-monitor system every other connector is excluded so only the
//!    target is blanked.
//!
//! ### All-output DPMS (fallback)
//!
//! When `output` is `None`, run `kscreen-doctor --dpms off` with **no**
//! exclusions — blanks every output.  This is audio-unsafe and should only
//! be used when no output-specific config is available.
//!
//! A future path may use `org.kde.KWin` `DBus` for per-output control, but the
//! shipped mechanism is `kscreen-doctor` (verified per-output on Plasma 6.7.2).

use std::env;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::{DormantError, E_DISPLAY_IO};
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Binary name — literal anchor for PATH checks and error messages.
const KSCREEN_DOCTOR: &str = "kscreen-doctor";

/// Maximum stderr bytes surfaced in a `CmdFailure` on non-zero exit.
const STDERR_TAIL: usize = 200;

// ── Argument construction (pure, testable) ─────────────────────────────────────

/// Build the argument list for a `kscreen-doctor` DPMS command using the
/// `--dpms-excluded` per-output scoping mechanism.
///
/// `kscreen-doctor --dpms <off|on>` is a **global** flag; per-output control
/// is achieved by excluding every other connector:
///
/// ```text
/// kscreen-doctor --dpms off --dpms-excluded HDMI-A-1 --dpms-excluded DP-2
/// ```
///
/// # Arguments
///
/// - `target`: the connector to affect, or `None` for all-output.
/// - `all_outputs`: every connector name known to the compositor
///   (from `kscreen-doctor -o`).
/// - `on`: `true` → wake (`on`), `false` → blank (`off`).
///
/// # Precondition
///
/// When `target` is `Some(t)`, `t` must be present in `all_outputs`.
/// The caller validates this before calling `dpms_args`; this function
/// does not check.
///
/// # Examples
///
/// ```
/// # use dormant_displays::kwin_dpms::dpms_args;
/// // Single-monitor system: no exclusions.
/// assert_eq!(
///     dpms_args(Some("DP-1"), &["DP-1"], false),
///     vec!["--dpms", "off"]
/// );
///
/// // Multi-monitor: exclusions for the two other outputs.
/// assert_eq!(
///     dpms_args(Some("DP-1"), &["DP-1", "HDMI-A-1", "DP-2"], true),
///     vec!["--dpms", "on", "--dpms-excluded", "HDMI-A-1", "--dpms-excluded", "DP-2"]
/// );
///
/// // All-output: no target, no exclusions.
/// assert_eq!(
///     dpms_args(None, &[], false),
///     vec!["--dpms", "off"]
/// );
/// ```
#[must_use]
pub fn dpms_args(target: Option<&str>, all_outputs: &[&str], on: bool) -> Vec<String> {
    let mut args = vec![
        "--dpms".to_string(),
        if on { "on" } else { "off" }.to_string(),
    ];

    if let Some(t) = target {
        for output in all_outputs {
            if *output != t {
                args.push("--dpms-excluded".to_string());
                args.push((*output).to_string());
            }
        }
    }
    // target == None → no exclusions (all-output)

    args
}

// ── DpmsTransport trait ────────────────────────────────────────────────────────

/// Abstraction over the `kscreen-doctor` subprocess so unit tests can inject a
/// fake instead of requiring a real `KWin` Wayland session.
#[async_trait]
pub trait DpmsTransport: Send + Sync {
    /// Check whether the transport can reach `kscreen-doctor`.
    async fn check_available(&self) -> bool;

    /// Enumerate connector names from `kscreen-doctor -o`, or an empty list
    /// if enumeration is not meaningful.
    async fn list_outputs(&self) -> Result<Vec<String>, DormantError>;

    /// Run `kscreen-doctor` with the given arguments (already including
    /// `--dpms on|off …`).
    async fn execute(&self, args: &[String]) -> Result<(), CmdFailure>;
}

// ── Fake transport for tests ───────────────────────────────────────────────────

/// A fake `DpmsTransport` with configurable availability, output list, and
/// execution results.
///
/// Construct via [`FakeDpmsTransport::new`] and configure with the `set_*`
/// methods.
pub struct FakeDpmsTransport {
    available: std::sync::Mutex<bool>,
    outputs: std::sync::Mutex<Vec<String>>,
    result: std::sync::Mutex<Result<(), CmdFailure>>,
}

impl FakeDpmsTransport {
    /// Create a new fake that is available, has two outputs, and succeeds by default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            available: std::sync::Mutex::new(true),
            outputs: std::sync::Mutex::new(vec!["DP-1".to_string(), "HDMI-A-1".to_string()]),
            result: std::sync::Mutex::new(Ok(())),
        }
    }

    /// Set the return value of `check_available`.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`Mutex`](std::sync::Mutex) is poisoned.
    pub fn set_available(&self, available: bool) {
        *self.available.lock().unwrap() = available;
    }

    /// Set the output list returned by `list_outputs`.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`Mutex`](std::sync::Mutex) is poisoned.
    pub fn set_outputs(&self, outputs: Vec<String>) {
        *self.outputs.lock().unwrap() = outputs;
    }

    /// Set the return value of `execute`.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`Mutex`](std::sync::Mutex) is poisoned.
    pub fn set_result(&self, result: Result<(), CmdFailure>) {
        *self.result.lock().unwrap() = result;
    }
}

impl Default for FakeDpmsTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DpmsTransport for FakeDpmsTransport {
    async fn check_available(&self) -> bool {
        *self.available.lock().unwrap()
    }

    async fn list_outputs(&self) -> Result<Vec<String>, DormantError> {
        Ok(self.outputs.lock().unwrap().clone())
    }

    async fn execute(&self, _args: &[String]) -> Result<(), CmdFailure> {
        self.result.lock().unwrap().clone()
    }
}

// ── Real transport (Linux only) ────────────────────────────────────────────────

/// The real `kscreen-doctor` subprocess transport.
///
/// Only available on Linux; `kscreen-doctor` is a KDE/Wayland tool.
#[cfg(target_os = "linux")]
pub struct KscreenDoctorTransport {
    timeout: Duration,
}

#[cfg(target_os = "linux")]
impl KscreenDoctorTransport {
    /// Create a new real transport.
    ///
    /// `timeout` bounds internal subprocess calls (`-o` enumeration,
    /// `--help` availability check).  The controller additionally wraps
    /// `execute()` with the same timeout for blank/wake commands.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl DpmsTransport for KscreenDoctorTransport {
    async fn check_available(&self) -> bool {
        // WAYLAND_DISPLAY must be set for kscreen-doctor to reach the compositor.
        if env::var("WAYLAND_DISPLAY").is_err() {
            return false;
        }

        // Quick sanity: can we spawn kscreen-doctor?  --help exits 0 and is fast.
        let result = tokio::process::Command::new(KSCREEN_DOCTOR)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();

        match result {
            Ok(mut child) => match tokio::time::timeout(self.timeout, child.wait()).await {
                Ok(Ok(status)) => status.success(),
                _ => false,
            },
            Err(_) => false,
        }
    }

    async fn list_outputs(&self) -> Result<Vec<String>, DormantError> {
        let child = tokio::process::Command::new(KSCREEN_DOCTOR)
            .arg("-o")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DormantError::DisplayIo {
                controller: "kwin-dpms".into(),
                detail: format!("{E_DISPLAY_IO}: spawn failed: {e}"),
            })?;

        let wait_outcome = tokio::time::timeout(self.timeout, child.wait_with_output()).await;

        match wait_outcome {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let tail = truncate_utf8_tail(&stderr, STDERR_TAIL);
                    return Err(DormantError::DisplayIo {
                        controller: "kwin-dpms".into(),
                        detail: format!("{E_DISPLAY_IO}: -o failed: {tail}"),
                    });
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                Ok(parse_connectors(&stdout))
            }
            Ok(Err(e)) => Err(DormantError::DisplayIo {
                controller: "kwin-dpms".into(),
                detail: format!("{E_DISPLAY_IO}: wait failed: {e}"),
            }),
            Err(_elapsed) => Err(DormantError::DisplayIo {
                controller: "kwin-dpms".into(),
                detail: format!("{E_DISPLAY_IO}: timeout after {:?}", self.timeout),
            }),
        }
    }

    async fn execute(&self, args: &[String]) -> Result<(), CmdFailure> {
        // No internal timeout here — the controller's `execute_with_timeout`
        // wrapper applies the timeout.  The subprocess itself uses
        // kill_on_drop so the OS cleans up the child on cancellation.
        let child = tokio::process::Command::new(KSCREEN_DOCTOR)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| CmdFailure {
                controller: "kwin-dpms".to_string(),
                error: format!("{E_DISPLAY_IO}: spawn failed: {e}"),
            })?;

        let output = child.wait_with_output().await.map_err(|e| CmdFailure {
            controller: "kwin-dpms".to_string(),
            error: format!("{E_DISPLAY_IO}: wait failed: {e}"),
        })?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail = truncate_utf8_tail(&stderr, STDERR_TAIL);
            let detail = match output.status.code() {
                Some(c) => format!("exit code {c}; stderr: {tail}"),
                None => format!("terminated by signal; stderr: {tail}"),
            };
            Err(CmdFailure {
                controller: "kwin-dpms".to_string(),
                error: format!("{E_DISPLAY_IO}: {detail}"),
            })
        }
    }
}

/// Parse connector names from `kscreen-doctor -o` stdout.
///
/// Lines look like: `Output: 1 DP-1 uuid ...`.  The second whitespace-delimited
/// field after `Output:` is the connector name.
#[cfg(target_os = "linux")]
fn parse_connectors(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let stripped = line.strip_prefix("Output:")?;
            let parts: Vec<&str> = stripped.split_whitespace().collect();
            // parts[0] = index, parts[1] = connector name (if present)
            if parts.len() >= 2 {
                Some(parts[1].to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Keep the last `max` characters (best-effort) of `s` as valid UTF-8.
///
/// "Tail" semantics: the diagnostic usually lives at the end of stderr.
fn truncate_utf8_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let target = s.len().saturating_sub(max);
    let mut idx = target;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    s[idx..].to_string()
}

// ── KwinDpmsController ─────────────────────────────────────────────────────────

/// Display controller that blanks/wakes displays via `KWin` `kscreen-doctor` DPMS.
///
/// ## Positions on the fallback chain
///
/// This is an **audio-unsafe** controller — DPMS off tears down the output's
/// audio sink.  Place it after `ddcci` (audio-safe) in the controller chain
/// so it only fires for outputs with no DDC/CI support.
///
/// ## Output targeting
///
/// - `output: Some("DP-1")` — per-output DPMS via `--dpms-excluded`
///   (only blanks the target output, excluding all others).
/// - `output: None` — all-output DPMS (audio-unsafe fallback; use only when
///   no output name is known).
pub struct KwinDpmsController {
    output: Option<String>,
    timeout: Duration,
    transport: Arc<dyn DpmsTransport>,
}

impl KwinDpmsController {
    /// Build a `KwinDpmsController` with the real `kscreen-doctor` transport.
    ///
    /// `timeout` bounds every subprocess invocation (passed through from
    /// [`DisplayConfig::command_timeout`](dormant_core::config::schema::DisplayConfig::command_timeout)).
    ///
    /// Only available on Linux.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn new(output: Option<String>, timeout: Duration) -> Self {
        Self {
            output,
            timeout,
            transport: Arc::new(KscreenDoctorTransport::new(timeout)),
        }
    }

    /// Build a `KwinDpmsController` with a custom `DpmsTransport`
    /// (used by tests to inject a fake).
    #[must_use]
    pub fn with_transport(
        output: Option<String>,
        timeout: Duration,
        transport: Arc<dyn DpmsTransport>,
    ) -> Self {
        Self {
            output,
            timeout,
            transport,
        }
    }
}

impl KwinDpmsController {
    /// Literal controller name — grep-stable, matches the `kwin-dpms` config type.
    const NAME: &'static str = "kwin-dpms";

    /// Run `execute` with the controller's timeout, converting a timeout
    /// into a `CmdFailure`.
    async fn execute_with_timeout(&self, args: &[String]) -> Result<(), CmdFailure> {
        match tokio::time::timeout(self.timeout, self.transport.execute(args)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!("{E_DISPLAY_IO}: timeout after {:?}", self.timeout),
            }),
        }
    }
}

#[async_trait]
impl DisplayController for KwinDpmsController {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        vec![BlankMode::PowerOff]
    }

    async fn is_available(&self) -> bool {
        self.transport.check_available().await
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        if mode != BlankMode::PowerOff {
            return Err(CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!(
                    "{E_DISPLAY_IO}: unsupported blank mode {mode:?} for {name}",
                    name = Self::NAME
                ),
            });
        }

        let args = match &self.output {
            Some(target) => {
                let all = self
                    .transport
                    .list_outputs()
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to enumerate outputs: {e}"),
                    })?;

                if !all.iter().any(|o| o == target) {
                    return Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!(
                            "{E_DISPLAY_IO}: configured output '{target}' not found \
                             (known: {known})",
                            known = all.join(", ")
                        ),
                    });
                }

                let refs: Vec<&str> = all.iter().map(String::as_str).collect();
                dpms_args(Some(target), &refs, false)
            }
            None => dpms_args(None, &[], false),
        };

        self.execute_with_timeout(&args).await
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let args = match &self.output {
            Some(target) => {
                let all = self
                    .transport
                    .list_outputs()
                    .await
                    .map_err(|e| CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: failed to enumerate outputs: {e}"),
                    })?;

                // On wake, be lenient: if the target isn't in the list (e.g.
                // the output was physically disconnected), fall back to
                // all-output DPMS so we don't stay blanked.
                if !all.iter().any(|o| o == target) {
                    // Logged at info: this is a recoverable condition, not an
                    // error that should fail the wake.
                    tracing::info!(
                        event = "kwin_wake_target_missing",
                        target = %target,
                        known = %all.join(", "),
                        "configured output not found in kscreen-doctor -o; \
                         falling back to all-output wake"
                    );
                    let fallback = dpms_args(None, &[], true);
                    return self.execute_with_timeout(&fallback).await;
                }

                let refs: Vec<&str> = all.iter().map(String::as_str).collect();
                dpms_args(Some(target), &refs, true)
            }
            None => dpms_args(None, &[], true),
        };

        self.execute_with_timeout(&args).await
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    // ── dpms_args unit tests ────────────────────────────────────────────────

    #[test]
    fn dpms_args_single_output_no_exclusions() {
        // Single-monitor system: target is the only output → no --dpms-excluded.
        let args = dpms_args(Some("DP-1"), &["DP-1"], false);
        assert_eq!(args, vec!["--dpms", "off"]);
    }

    #[test]
    fn dpms_args_single_output_wake() {
        let args = dpms_args(Some("DP-1"), &["DP-1"], true);
        assert_eq!(args, vec!["--dpms", "on"]);
    }

    #[test]
    fn dpms_args_multi_output_excludes_others() {
        // Two other outputs → two --dpms-excluded pairs.
        let args = dpms_args(Some("DP-1"), &["DP-1", "HDMI-A-1", "DP-2"], false);
        assert_eq!(
            args,
            vec![
                "--dpms",
                "off",
                "--dpms-excluded",
                "HDMI-A-1",
                "--dpms-excluded",
                "DP-2"
            ]
        );
    }

    #[test]
    fn dpms_args_multi_output_only_one_other() {
        let args = dpms_args(Some("DP-1"), &["DP-1", "HDMI-A-1"], true);
        assert_eq!(args, vec!["--dpms", "on", "--dpms-excluded", "HDMI-A-1"]);
    }

    #[test]
    fn dpms_args_none_target_no_exclusions() {
        let args = dpms_args(None, &[], false);
        assert_eq!(args, vec!["--dpms", "off"]);
    }

    #[test]
    fn dpms_args_none_target_wake_no_exclusions() {
        let args = dpms_args(None, &[], true);
        assert_eq!(args, vec!["--dpms", "on"]);
    }

    // ── Controller tests (with FakeDpmsTransport) ───────────────────────────

    fn make_controller(output: Option<&str>) -> (KwinDpmsController, Arc<FakeDpmsTransport>) {
        let fake = Arc::new(FakeDpmsTransport::new());
        let ctrl = KwinDpmsController::with_transport(
            output.map(String::from),
            Duration::from_secs(5),
            Arc::clone(&fake) as Arc<dyn DpmsTransport>,
        );
        (ctrl, fake)
    }

    #[test]
    fn name_returns_kwin_dpms() {
        let (ctrl, _) = make_controller(Some("DP-1"));
        assert_eq!(ctrl.name(), "kwin-dpms");
    }

    #[test]
    fn supported_modes_returns_power_off_only() {
        let (ctrl, _) = make_controller(Some("DP-1"));
        assert_eq!(ctrl.supported_modes(), vec![BlankMode::PowerOff]);
    }

    #[tokio::test]
    async fn is_available_delegates_to_transport() {
        let (ctrl, fake) = make_controller(Some("DP-1"));
        assert!(ctrl.is_available().await);

        fake.set_available(false);
        assert!(!ctrl.is_available().await);
    }

    #[tokio::test]
    async fn blank_unsupported_mode_errs() {
        let (ctrl, _fake) = make_controller(Some("DP-1"));
        let err = ctrl.blank(BlankMode::ScreenOffAudioOn).await.unwrap_err();
        assert_eq!(err.controller, "kwin-dpms");
        assert!(err.error.contains("unsupported"));
        assert!(err.error.contains("ScreenOffAudioOn"));
    }

    #[tokio::test]
    async fn blank_brightness_zero_errs() {
        let (ctrl, _fake) = make_controller(Some("DP-1"));
        let err = ctrl.blank(BlankMode::BrightnessZero).await.unwrap_err();
        assert_eq!(err.controller, "kwin-dpms");
        assert!(err.error.contains("unsupported"));
    }

    #[tokio::test]
    async fn blank_propagates_transport_error() {
        let (ctrl, fake) = make_controller(Some("DP-1"));
        fake.set_result(Err(CmdFailure {
            controller: "kwin-dpms".to_string(),
            error: format!("{E_DISPLAY_IO}: simulated failure"),
        }));
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(err.error.contains("simulated failure"));
    }

    #[tokio::test]
    async fn wake_propagates_transport_error() {
        let (ctrl, fake) = make_controller(Some("DP-1"));
        fake.set_result(Err(CmdFailure {
            controller: "kwin-dpms".to_string(),
            error: format!("{E_DISPLAY_IO}: simulated wake failure"),
        }));
        let err = ctrl.wake().await.unwrap_err();
        assert!(err.error.contains("simulated wake failure"));
    }

    // ── Per-output blank/wake (exclusion mechanism) ─────────────────────────

    #[tokio::test]
    async fn blank_single_output_no_exclusions_called() {
        // Default fake has outputs ["DP-1", "HDMI-A-1"]; target "DP-1"
        // → should exclude "HDMI-A-1".
        let (ctrl, _fake) = make_controller(Some("DP-1"));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
    }

    #[tokio::test]
    async fn blank_target_not_found_errs() {
        let (ctrl, fake) = make_controller(Some("NONEXISTENT"));
        fake.set_outputs(vec!["DP-1".to_string(), "HDMI-A-1".to_string()]);
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        assert_eq!(err.controller, "kwin-dpms");
        assert!(
            err.error
                .contains("configured output 'NONEXISTENT' not found")
        );
        assert!(err.error.contains("DP-1"));
    }

    #[tokio::test]
    async fn blank_none_output_all_output_fallback() {
        // None target → all-output, no enumeration needed.
        let (ctrl, _fake) = make_controller(None);
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
    }

    #[tokio::test]
    async fn wake_single_output_succeeds() {
        let (ctrl, _fake) = make_controller(Some("DP-1"));
        ctrl.wake().await.unwrap();
    }

    #[tokio::test]
    async fn wake_none_output_succeeds() {
        let (ctrl, _fake) = make_controller(None);
        ctrl.wake().await.unwrap();
    }

    // ── Timeout regression test (SHOULD 2) ──────────────────────────────────

    /// Verifies that a subprocess that hangs does not hang the caller — the
    /// controller's `execute_with_timeout` wrapper fires, and `blank()`
    /// returns a `CmdFailure` (not a hang).
    #[tokio::test]
    async fn blank_timeout_returns_error_not_hang() {
        let ctrl = KwinDpmsController::with_transport(
            Some("DP-1".into()),
            Duration::from_millis(200),
            Arc::new(HangingTransport),
        );

        let start = std::time::Instant::now();
        let err = ctrl.blank(BlankMode::PowerOff).await.unwrap_err();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "blank must be bounded by timeout; took {elapsed:?}"
        );
        assert_eq!(err.controller, "kwin-dpms");
        assert!(err.error.contains("timeout"));
    }

    #[tokio::test]
    async fn wake_timeout_returns_error_not_hang() {
        let ctrl = KwinDpmsController::with_transport(
            Some("DP-1".into()),
            Duration::from_millis(200),
            Arc::new(HangingTransport),
        );

        let start = std::time::Instant::now();
        let err = ctrl.wake().await.unwrap_err();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "wake must be bounded by timeout; took {elapsed:?}"
        );
        assert_eq!(err.controller, "kwin-dpms");
        assert!(err.error.contains("timeout"));
    }

    /// A transport whose `execute` sleeps forever, so the controller's
    /// `execute_with_timeout` wrapper fires.  Proves the timeout path is
    /// exercised and bounded.
    struct HangingTransport;

    #[async_trait]
    impl DpmsTransport for HangingTransport {
        async fn check_available(&self) -> bool {
            true
        }

        async fn list_outputs(&self) -> Result<Vec<String>, DormantError> {
            Ok(vec!["DP-1".to_string()])
        }

        async fn execute(&self, _args: &[String]) -> Result<(), CmdFailure> {
            // Sleep longer than any reasonable controller timeout.
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
    }
}
