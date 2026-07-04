//! `command` display controller — runs a user-supplied shell command for blank
//! and another for wake. The `is_available()` claim is optimistic: the config
//! asserting `blank_command` + `wake_command` is treated as evidence the
//! controller is configured; a missing binary or non-zero exit surfaces at use
//! as a `E_DISPLAY_IO` `CmdFailure`.
//!
//! Both blank and wake invocations are wrapped in
//! [`tokio::time::timeout`] using `DisplayConfig::command_timeout` so that a
//! hung command cannot wedge the executor's bounded retry burst.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

/// Maximum number of stderr bytes appended to the `CmdFailure` detail on a
/// non-zero exit. Capped so a chatty failing command does not bloat log lines
/// or IPC payloads.
const STDERR_TAIL: usize = 200;

/// Display controller that executes arbitrary shell commands for blank and wake.
///
/// Constructed by [`crate::registry::build_controllers`] from a
/// [`dormant_core::config::schema::DisplayConfig`] that names `command` as one
/// of its controllers.
pub struct CommandController {
    blank_command: String,
    wake_command: String,
    modes: Vec<BlankMode>,
    timeout: Duration,
}

impl CommandController {
    /// Build a new `CommandController` from the four validated config fields.
    #[must_use]
    pub fn new(
        blank_command: String,
        wake_command: String,
        modes: Vec<BlankMode>,
        timeout: Duration,
    ) -> Self {
        Self {
            blank_command,
            wake_command,
            modes,
            timeout,
        }
    }

    /// Run `command` via `sh -c`, honoring `timeout`.
    async fn run_shell(&self, command: &str) -> Result<(), CmdFailure> {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!("{E_DISPLAY_IO}: spawn failed: {e}"),
            })?;

        let stderr_tail: String;
        let exit_status: std::io::Result<std::process::ExitStatus>;

        match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => {
                exit_status = Ok(status);
                stderr_tail = read_stderr_tail(&mut child).await;
            }
            Ok(Err(io)) => {
                return Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!("{E_DISPLAY_IO}: wait failed: {io}"),
                });
            }
            Err(_elapsed) => {
                // kill_on_drop(true) ensures the child is reaped when the
                // Command value is dropped; no explicit kill needed.
                return Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!("{E_DISPLAY_IO}: timeout after {:?}", self.timeout),
                });
            }
        }

        let status = exit_status.map_err(|e| CmdFailure {
            controller: Self::NAME.to_string(),
            error: format!("{E_DISPLAY_IO}: wait failed: {e}"),
        })?;

        if status.success() {
            return Ok(());
        }

        let code = status.code();
        let detail = match code {
            Some(c) => format!("exit code {c}; stderr: {stderr_tail}"),
            None => format!("terminated by signal; stderr: {stderr_tail}"),
        };
        Err(CmdFailure {
            controller: Self::NAME.to_string(),
            error: format!("{E_DISPLAY_IO}: {detail}"),
        })
    }
}

impl CommandController {
    /// Literal controller name — grep-stable, matches the `command` config type.
    const NAME: &'static str = "command";
}

#[async_trait]
impl DisplayController for CommandController {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supported_modes(&self) -> Vec<BlankMode> {
        self.modes.clone()
    }

    async fn is_available(&self) -> bool {
        // The config's presence of both commands is treated as the availability
        // assertion; a missing binary surfaces as exit failure at first use.
        true
    }

    async fn blank(&self, mode: BlankMode) -> Result<(), CmdFailure> {
        if !self.modes.contains(&mode) {
            return Err(CmdFailure {
                controller: Self::NAME.to_string(),
                error: format!(
                    "{E_DISPLAY_IO}: mode {mode:?} not in declared modes {:?}",
                    self.modes
                ),
            });
        }
        self.run_shell(&self.blank_command).await
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        self.run_shell(&self.wake_command).await
    }
}

async fn read_stderr_tail(child: &mut tokio::process::Child) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
    truncate_utf8(&buf, STDERR_TAIL)
}

/// Keep the last `max` *characters* (best-effort) of `bytes` as valid UTF-8.
///
/// "Tail" semantics: the end of stderr usually carries the diagnostic that
/// matters, so when truncating we drop the *prefix* and keep the suffix.
fn truncate_utf8(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= max {
        return s.into_owned();
    }
    // Start slicing at the largest char boundary ≤ (len - max) so the
    // returned string contains exactly the last `max` characters (or fewer if
    // multi-byte chars straddle the boundary).
    let target = s.len().saturating_sub(max);
    let mut idx = target;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    s[idx..].to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use dormant_core::error::E_DISPLAY_IO;

    #[tokio::test]
    async fn exit0_ok() {
        let c = CommandController::new(
            "true".into(),
            "true".into(),
            vec![BlankMode::PowerOff],
            Duration::from_secs(5),
        );
        c.blank(BlankMode::PowerOff).await.unwrap();
        c.wake().await.unwrap();
    }

    #[tokio::test]
    async fn exit1_err_with_code_and_stderr() {
        let c = CommandController::new(
            "echo boom >&2; exit 7".into(),
            "true".into(),
            vec![BlankMode::PowerOff],
            Duration::from_secs(5),
        );
        let err = c.blank(BlankMode::PowerOff).await.unwrap_err();
        assert_eq!(err.controller, "command");
        assert!(
            err.error.starts_with(E_DISPLAY_IO),
            "error must start with {E_DISPLAY_IO}: {}",
            err.error
        );
        assert!(
            err.error.contains("exit code 7"),
            "error should mention exit code 7: {}",
            err.error
        );
        assert!(
            err.error.contains("boom"),
            "error should include stderr tail: {}",
            err.error
        );
    }

    #[tokio::test]
    async fn sleep_past_timeout_errs() {
        let c = CommandController::new(
            "sleep 5".into(),
            "true".into(),
            vec![BlankMode::PowerOff],
            Duration::from_millis(200),
        );
        let start = std::time::Instant::now();
        let err = c.blank(BlankMode::PowerOff).await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "blank should be bounded by timeout (~200ms), took {elapsed:?}"
        );
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains("timeout"));
    }

    #[tokio::test]
    async fn mode_not_declared_rejected() {
        let c = CommandController::new(
            "true".into(),
            "true".into(),
            vec![BlankMode::ScreenOffAudioOn],
            Duration::from_secs(5),
        );
        let err = c.blank(BlankMode::PowerOff).await.unwrap_err();
        assert_eq!(err.controller, "command");
        assert!(err.error.contains("not in declared modes"));
    }

    #[tokio::test]
    async fn stderr_tail_keeps_last_200_chars() {
        // A long failing command: the tail of stderr (the diagnostic that
        // matters) is preserved; earlier bytes are dropped.
        let prefix = "x".repeat(500);
        let suffix = "END_OF_FAILURE";
        let c = CommandController::new(
            format!("printf '{prefix}' >&2; printf '{suffix}' >&2; exit 1"),
            "true".into(),
            vec![BlankMode::PowerOff],
            Duration::from_secs(5),
        );
        let err = c.blank(BlankMode::PowerOff).await.unwrap_err();
        // The diagnostic suffix at the tail survives the cap.
        assert!(
            err.error.contains(suffix),
            "tail marker should be present: {}",
            err.error
        );
        // The embedded stderr chunk is bounded to at most STDERR_TAIL chars.
        let stderr_start = err.error.find("stderr: ").unwrap() + "stderr: ".len();
        let stderr_chunk = &err.error[stderr_start..];
        assert!(
            stderr_chunk.len() <= STDERR_TAIL,
            "stderr chunk should be <= {STDERR_TAIL} chars, got {}",
            stderr_chunk.len()
        );
    }
}
