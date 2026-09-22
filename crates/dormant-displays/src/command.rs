//! `command` display controller — runs a user-supplied shell command for blank
//! and another for wake. The `is_available()` claim is optimistic: the config
//! asserting `blank_command` + `wake_command` is treated as evidence the
//! controller is configured; a missing binary or non-zero exit surfaces at use
//! as a `E_DISPLAY_IO` `CmdFailure`.
//!
//! Both blank and wake invocations are wrapped in
//! [`tokio::time::timeout`] using `DisplayConfig::command_timeout` so that a
//! hung command cannot wedge the executor's bounded retry burst.
//!
//! ## Why a concurrent stderr drain
//!
//! The OS pipe buffer for a child's stderr is finite (~64 KiB on Linux). A
//! command that writes more than that without anyone reading the read end
//! blocks on `write(2)`, which means `child.wait()` never observes the exit
//! and the executor's timeout fires for an otherwise-successful command. We
//! drain stderr on a separate task while the main task awaits `child.wait()`
//! — the kernel pipe stays below capacity, the child exits promptly, and the
//! drain task observes EOF and returns. The drain task bounds its in-memory
//! buffer to the last 4 KiB so a 1 GiB flood cannot OOM the daemon.
//!
//! On timeout, the child is killed explicitly via `start_kill()` (not just
//! `kill_on_drop`) so the failure surface is identical whether or not the
//! `Command` value is dropped promptly by the caller.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::error::E_DISPLAY_IO;
use dormant_core::traits::DisplayController;
use dormant_core::types::{BlankMode, CmdFailure};

/// Maximum number of stderr bytes appended to the `CmdFailure` detail on a
/// non-zero exit. Capped so a chatty failing command does not bloat log lines
/// or IPC payloads.
const STDERR_TAIL: usize = 200;

/// Cap on the in-memory stderr buffer kept by the drain task. Keeps memory
/// bounded even if the command floods stderr with hundreds of MiB. The drain
/// drops the *prefix* so the diagnostic at the tail survives.
const STDERR_DRAIN_CAP: usize = 4096;

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

    /// Run `command` through the platform shell, honoring `timeout` and
    /// draining stderr concurrently so a verbose failure doesn't deadlock on
    /// the OS pipe buffer.
    ///
    /// `sh -c` on Unix, `cmd /C` on Windows. The shell is chosen per platform
    /// rather than hard-coded: `sh` does not exist on a stock Windows install,
    /// so a `command` controller there previously validated fine and then
    /// failed every blank and wake with a spawn error. Operators write commands
    /// in their own platform's shell syntax either way.
    async fn run_shell(&self, command: &str) -> Result<(), CmdFailure> {
        #[cfg(windows)]
        let (shell, shell_flag) = ("cmd", "/C");
        #[cfg(not(windows))]
        let (shell, shell_flag) = ("sh", "-c");

        let mut child = tokio::process::Command::new(shell)
            .arg(shell_flag)
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

        // Pull stderr out so the drain task owns it; the main task keeps the
        // Child for wait/kill. If we couldn't get the pipe (e.g. spawn
        // configured stderr differently), proceed without a drain — the
        // existing single-thread drain path would also block on overflow.
        let stderr = child.stderr.take();

        let stderr_buf: Arc<StdMutex<Vec<u8>>> = Arc::new(StdMutex::new(Vec::new()));

        let drain_handle = stderr.map(|mut stderr_pipe| {
            let buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut local: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut stderr_pipe, &mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            local.extend_from_slice(&chunk[..n]);
                            // Bound the buffer to the last STDERR_DRAIN_CAP
                            // bytes — the tail is what carries the diagnostic.
                            if local.len() > STDERR_DRAIN_CAP {
                                let drop = local.len() - STDERR_DRAIN_CAP;
                                local.drain(..drop);
                            }
                        }
                    }
                }
                if let Ok(mut g) = buf.lock() {
                    *g = local;
                }
            })
        });

        let wait_outcome = tokio::time::timeout(self.timeout, child.wait()).await;

        match wait_outcome {
            Ok(Ok(status)) => {
                // Drain task will hit EOF on its next read; await it so the
                // buffer is populated before we slice the tail.
                if let Some(h) = drain_handle {
                    let _ = h.await;
                }
                let stderr_bytes = stderr_buf.lock().map(|g| g.clone()).unwrap_or_default();
                let stderr_tail = truncate_utf8(&stderr_bytes, STDERR_TAIL);

                if status.success() {
                    Ok(())
                } else {
                    let detail = match status.code() {
                        Some(c) => format!("exit code {c}; stderr: {stderr_tail}"),
                        None => format!("terminated by signal; stderr: {stderr_tail}"),
                    };
                    Err(CmdFailure {
                        controller: Self::NAME.to_string(),
                        error: format!("{E_DISPLAY_IO}: {detail}"),
                    })
                }
            }
            Ok(Err(io)) => {
                if let Some(h) = drain_handle {
                    h.abort();
                }
                Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!("{E_DISPLAY_IO}: wait failed: {io}"),
                })
            }
            Err(_elapsed) => {
                // Timeout — kill the child explicitly (not via kill_on_drop
                // alone) so the wait surface is uniform regardless of whether
                // the Command value is dropped promptly.
                let _ = child.start_kill();
                let _ = child.wait().await;
                if let Some(h) = drain_handle {
                    // Killing the child closes its stderr fd; the drain task
                    // will then see EOF and exit. Give it a chance to finish
                    // so we don't leak the task.
                    let _ = h.await;
                }
                Err(CmdFailure {
                    controller: Self::NAME.to_string(),
                    error: format!("{E_DISPLAY_IO}: timeout after {:?}", self.timeout),
                })
            }
        }
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

    /// Shell fixtures, written in the syntax of the shell the controller
    /// actually uses: `sh -c` on Unix, `cmd /C` on Windows.
    ///
    /// Each helper produces the SAME observable behaviour on both platforms,
    /// so the assertions never relax to a common denominator — only the
    /// spelling differs.
    ///
    /// Windows fixtures deliberately contain no `"` characters. Rust escapes
    /// arguments by argv rules even when the program is `cmd`, so an embedded
    /// quote arrives as `\"`, which `cmd` does not understand. Quote-free
    /// strings are safe: with `/C`, `cmd` strips the leading and trailing
    /// quote it was given and runs the remainder verbatim.
    mod shell {
        /// Succeed with no output.
        pub fn ok() -> String {
            if cfg!(windows) {
                "exit 0".into()
            } else {
                "true".into()
            }
        }

        /// Write `msg` to stderr, then exit with `code`.
        pub fn stderr_then_exit(msg: &str, code: i32) -> String {
            if cfg!(windows) {
                format!("echo {msg} 1>&2 & exit {code}")
            } else {
                format!("echo {msg} >&2; exit {code}")
            }
        }

        /// A command that keeps running for at least `secs` seconds, so a
        /// controller timeout shorter than that is guaranteed to fire.
        ///
        /// Not a raw sleep — this builds a command STRING for the child
        /// process; no test thread blocks here.
        ///
        /// `ping -n` rather than `timeout /t` on Windows: the controller gives
        /// the child a null stdin, and `timeout` refuses to run when input is
        /// redirected.
        pub fn long_running(secs: u32) -> String {
            if cfg!(windows) {
                format!("ping -n {} 127.0.0.1 >nul", secs + 1)
            } else {
                format!("sleep {secs}")
            }
        }

        /// Write `first` then `second` to stderr, then exit with `code`.
        pub fn stderr_pair_then_exit(first: &str, second: &str, code: i32) -> String {
            if cfg!(windows) {
                format!("echo {first} 1>&2 & echo {second} 1>&2 & exit {code}")
            } else {
                format!("printf '{first}' >&2; printf '{second}' >&2; exit {code}")
            }
        }
    }

    #[tokio::test]
    async fn exit0_ok() {
        let c = CommandController::new(
            shell::ok(),
            shell::ok(),
            vec![BlankMode::PowerOff],
            Duration::from_secs(5),
        );
        c.blank(BlankMode::PowerOff).await.unwrap();
        c.wake().await.unwrap();
    }

    #[tokio::test]
    async fn exit1_err_with_code_and_stderr() {
        let c = CommandController::new(
            shell::stderr_then_exit("boom", 7),
            shell::ok(),
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
            shell::long_running(5),
            shell::ok(),
            vec![BlankMode::PowerOff],
            Duration::from_millis(200),
        );
        let err = c.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains("timeout"));
    }

    #[tokio::test]
    async fn mode_not_declared_rejected() {
        let c = CommandController::new(
            shell::ok(),
            shell::ok(),
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
            shell::stderr_pair_then_exit(&prefix, suffix, 1),
            shell::ok(),
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

    /// Must 1 — stderr pipe deadlock. A flood of >pipe-buffer bytes to stderr
    /// without concurrent draining used to wedge `child.wait()` and trip the
    /// timeout for an otherwise-successful command. With the concurrent drain
    /// in place the command completes normally; this is an outer-timeout I/O
    /// contract (the 3s bound is a ceiling, not a timing-window assertion).
    // Unix-only fixture: needs a shell that can emit >200 KiB to stderr in one
    // expression. `sh` has `yes X | head -c`; `cmd` has no compact equivalent,
    // and the `for /L`/`type` workarounds either need a quoted path (which
    // Rust's argv escaping mangles for `cmd`) or produce a non-deterministic
    // size that cannot anchor the tail-cap assertion. The DRAIN BEHAVIOUR
    // under test is portable and matters more on Windows, where pipe buffers
    // are smaller — tracked as follow-up, not settled as Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_flood_exit0_is_ok() {
        let c = CommandController::new(
            "yes X | head -c 200000 >&2; exit 0".into(),
            "true".into(),
            vec![BlankMode::PowerOff],
            Duration::from_secs(3),
        );
        let result = c.blank(BlankMode::PowerOff).await;
        assert!(
            result.is_ok(),
            "exit-0 command should succeed despite 200 KiB stderr flood; err={result:?}",
        );
    }

    /// Must 1 — non-zero exit after a stderr flood must still surface the
    /// diagnostic that mattered (a marker printed AFTER the flood).
    // Unix-only fixture: needs a shell that can emit >200 KiB to stderr in one
    // expression. `sh` has `yes X | head -c`; `cmd` has no compact equivalent,
    // and the `for /L`/`type` workarounds either need a quoted path (which
    // Rust's argv escaping mangles for `cmd`) or produce a non-deterministic
    // size that cannot anchor the tail-cap assertion. The DRAIN BEHAVIOUR
    // under test is portable and matters more on Windows, where pipe buffers
    // are smaller — tracked as follow-up, not settled as Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_flood_nonzero_keeps_tail() {
        let c = CommandController::new(
            "yes X | head -c 200000 >&2; echo MARKER >&2; exit 3".into(),
            "true".into(),
            vec![BlankMode::PowerOff],
            Duration::from_secs(3),
        );
        let err = c.blank(BlankMode::PowerOff).await.unwrap_err();
        assert!(err.error.starts_with(E_DISPLAY_IO));
        assert!(err.error.contains("exit code 3"));
        assert!(
            err.error.contains("MARKER"),
            "post-flood diagnostic should survive the 200-char tail cap: {}",
            err.error
        );
    }
}
