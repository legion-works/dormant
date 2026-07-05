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
//! ## Mechanism
//!
//! - **Per-output blank:** `kscreen-doctor --dpms off <output>` (e.g. `DP-1`).
//! - **Per-output wake:** `kscreen-doctor --dpms on <output>`.
//! - **All-output fallback:** when `output` is `None`, the output argument is
//!   omitted (`--dpms off` / `--dpms on` all outputs).  This is the
//!   **audio-unsafe** all-output path — use only when no output-specific config
//!   is available.
//!
//! A future path may use `org.kde.KWin` `DBus` for per-output control, but the
//! shipped mechanism is `kscreen-doctor` (verified per-output on Plasma 6.7.2).

use std::env;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Binary name — literal anchor for PATH checks and error messages.
const KSCREEN_DOCTOR: &str = "kscreen-doctor";

/// Maximum seconds to wait for a `kscreen-doctor` invocation.
/// Local `DBus` calls normally complete in <1s; 5s is generous headroom.
const KSCREEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum stderr bytes surfaced in a `CmdFailure` on non-zero exit.
const STDERR_TAIL: usize = 200;

// ── Argument construction (pure, testable) ─────────────────────────────────────

/// Build the argument list for a `kscreen-doctor` DPMS command.
///
/// - `output` of `Some("DP-1")` → `["--dpms", "off", "DP-1"]` (per-output).
/// - `output` of `None` → `["--dpms", "off"]` (all outputs — audio-unsafe).
/// - `on: true` → `"on"`, `on: false` → `"off"`.
#[must_use]
pub fn dpms_args(output: Option<&str>, on: bool) -> Vec<String> {
    let mut args = vec![
        "--dpms".to_string(),
        if on { "on" } else { "off" }.to_string(),
    ];
    if let Some(o) = output {
        args.push(o.to_string());
    }
    args
}

// ── DpmsTransport trait ────────────────────────────────────────────────────────

/// Abstraction over the `kscreen-doctor` subprocess so unit tests can inject a
/// fake instead of requiring a real `KWin` Wayland session.
#[async_trait]
pub trait DpmsTransport: Send + Sync {
    /// Check whether the transport can reach `kscreen-doctor`.
    async fn check_available(&self) -> bool;

    /// Run `kscreen-doctor` with the given arguments (already including
    /// `--dpms on|off …`).
    async fn execute(&self, args: &[String]) -> Result<(), CmdFailure>;
}

// ── Fake transport for tests ───────────────────────────────────────────────────

/// A fake `DpmsTransport` with configurable availability and execution results.
///
/// Construct via [`FakeDpmsTransport::new`] and configure with
/// [`FakeDpmsTransport::set_available`] /
/// [`FakeDpmsTransport::set_result`].
pub struct FakeDpmsTransport {
    available: std::sync::Mutex<bool>,
    result: std::sync::Mutex<Result<(), CmdFailure>>,
}

impl FakeDpmsTransport {
    /// Create a new fake that is available and succeeds by default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            available: std::sync::Mutex::new(true),
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

    async fn execute(&self, _args: &[String]) -> Result<(), CmdFailure> {
        self.result.lock().unwrap().clone()
    }
}

// ── Real transport (Linux only) ────────────────────────────────────────────────

/// The real `kscreen-doctor` subprocess transport.
///
/// Only available on Linux; `kscreen-doctor` is a KDE/Wayland tool.
#[cfg(target_os = "linux")]
pub struct KscreenDoctorTransport;

#[cfg(target_os = "linux")]
impl KscreenDoctorTransport {
    /// Create a new real transport.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "linux")]
impl Default for KscreenDoctorTransport {
    fn default() -> Self {
        Self::new()
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
            Ok(mut child) => match tokio::time::timeout(KSCREEN_TIMEOUT, child.wait()).await {
                Ok(Ok(status)) => status.success(),
                _ => false,
            },
            Err(_) => false,
        }
    }

    async fn execute(&self, args: &[String]) -> Result<(), CmdFailure> {
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

        // wait_with_output() takes ownership of the Child. On timeout the
        // future is cancelled, the Child is dropped, and kill_on_drop fires.
        let wait_outcome = tokio::time::timeout(KSCREEN_TIMEOUT, child.wait_with_output()).await;

        match wait_outcome {
            Ok(Ok(output)) => {
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
            Ok(Err(e)) => Err(CmdFailure {
                controller: "kwin-dpms".to_string(),
                error: format!("{E_DISPLAY_IO}: wait failed: {e}"),
            }),
            Err(_elapsed) => {
                // Child was moved into wait_with_output(); the future's drop
                // on timeout triggers kill_on_drop — no explicit kill needed.
                Err(CmdFailure {
                    controller: "kwin-dpms".to_string(),
                    error: format!("{E_DISPLAY_IO}: timeout after {KSCREEN_TIMEOUT:?}"),
                })
            }
        }
    }
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
/// - `output: Some("DP-1")` — per-output DPMS (preferred).
/// - `output: None` — all-output DPMS (audio-unsafe fallback; use only when
///   no output name is known).
pub struct KwinDpmsController {
    output: Option<String>,
    transport: Arc<dyn DpmsTransport>,
}

impl KwinDpmsController {
    /// Build a `KwinDpmsController` with the real `kscreen-doctor` transport.
    ///
    /// Only available on Linux.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn new(output: Option<String>) -> Self {
        Self {
            output,
            transport: Arc::new(KscreenDoctorTransport::new()),
        }
    }

    /// Build a `KwinDpmsController` with a custom `DpmsTransport`
    /// (used by tests to inject a fake).
    #[must_use]
    pub fn with_transport(output: Option<String>, transport: Arc<dyn DpmsTransport>) -> Self {
        Self { output, transport }
    }
}

impl KwinDpmsController {
    /// Literal controller name — grep-stable, matches the `kwin-dpms` config type.
    const NAME: &'static str = "kwin-dpms";
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
        let args = dpms_args(self.output.as_deref(), false);
        self.transport.execute(&args).await
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        let args = dpms_args(self.output.as_deref(), true);
        self.transport.execute(&args).await
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;

    // ── dpms_args unit tests ────────────────────────────────────────────────

    #[test]
    fn dpms_args_some_output_off() {
        let args = dpms_args(Some("DP-1"), false);
        assert_eq!(args, vec!["--dpms", "off", "DP-1"]);
    }

    #[test]
    fn dpms_args_some_output_on() {
        let args = dpms_args(Some("HDMI-2"), true);
        assert_eq!(args, vec!["--dpms", "on", "HDMI-2"]);
    }

    #[test]
    fn dpms_args_none_output_off() {
        let args = dpms_args(None, false);
        assert_eq!(args, vec!["--dpms", "off"]);
    }

    #[test]
    fn dpms_args_none_output_on() {
        let args = dpms_args(None, true);
        assert_eq!(args, vec!["--dpms", "on"]);
    }

    // ── Controller tests (with FakeDpmsTransport) ───────────────────────────

    fn make_controller(output: Option<&str>) -> (KwinDpmsController, Arc<FakeDpmsTransport>) {
        let fake = Arc::new(FakeDpmsTransport::new());
        let ctrl = KwinDpmsController::with_transport(
            output.map(String::from),
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
    async fn blank_power_off_succeeds() {
        let (ctrl, _fake) = make_controller(Some("DP-1"));
        ctrl.blank(BlankMode::PowerOff).await.unwrap();
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
    async fn wake_succeeds() {
        let (ctrl, _fake) = make_controller(Some("DP-1"));
        ctrl.wake().await.unwrap();
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
}
