//! DDC/CI display probe — enumerates displays over I²C, with an optional
//! `ddcutil` second opinion (#35).
//!
//! The backend here compiles on Linux and macOS (wherever `dormant_displays`'s
//! `RealVcp` is available — see `crates/dormant-displays/src/vcp_ops.rs`).
//! Wiring this probe into `dormantctl doctor`'s macOS output (the `lib.rs`
//! `probe_all_offline` call site and the `dormantctl` `doctor ddcci`
//! subcommand, both still Linux-gated) is Task 11 — this task only
//! broadens the shared backend so it compiles there.
//!

//! ## `ddcutil` second opinion
//!
//! `ddc-hi` (the library backing [`RealVcp`]) and the standalone `ddcutil`
//! CLI both enumerate DDC/CI displays over I²C, but through independent
//! code paths. When both are present they act as a cross-check: if `ddc-hi`
//! reports a display that `ddcutil` disagrees with (or vice versa), that
//! mismatch usually means a phantom/ghost I²C bus rather than a real
//! display, and is worth an operator's attention.
//!
//! `ddcutil` is purely advisory, though — it is an optional system package
//! this probe never installs or requires. Its absence, failure, or
//! disagreement with `ddc-hi` NEVER changes this probe's pass/fail verdict;
//! [`probe_ddcci_with`] computes that verdict from `ddc-hi` alone and only
//! appends the `ddcutil` view as extra detail. See [`DdcutilOps`] for the
//! command-runner seam and [`format_second_opinion`] for how each outcome
//! is rendered.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::types::ProbeResult;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use dormant_displays::ddc_lock::PanelLocks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use dormant_displays::vcp_ops::RealVcp;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use dormant_displays::vcp_ops::{VcpOps, VcpPriority};

/// Bounded budget for the advisory `ddcutil detect --brief` second opinion.
/// `ddcutil` walks I²C buses too, so a hung/rogue bus must never stall the
/// doctor probe waiting on it — the probe always resolves within this
/// budget plus the `ddc-hi` reads above it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const DDCUTIL_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of one bounded `ddcutil detect --brief` invocation.
///
/// Every variant is advisory input to [`format_second_opinion`]; none of
/// them ever flip [`probe_ddcci_with`]'s pass/fail verdict, which is decided
/// from the `ddc-hi` reads alone.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum DdcutilOutcome {
    /// The `ddcutil` executable is not on `PATH` (`io::ErrorKind::NotFound`).
    /// The overwhelmingly common case: `ddcutil` is an optional package most
    /// deployments never install.
    NotInstalled,
    /// The command ran to completion (any exit status) within the timeout.
    Completed {
        /// Whether the process exited with status 0.
        success: bool,
        stdout: String,
        stderr: String,
    },
    /// The command did not finish within [`DDCUTIL_TIMEOUT`].
    TimedOut,
    /// Some other spawn/IO failure (e.g. permission denied on the
    /// executable). Classified the same as a nonzero exit: advisory
    /// failure, never retried through a shell or with different arguments.
    SpawnError(String),
}

/// Seam over invoking `ddcutil` as an external process, so tests can script
/// every branch of the command-runner matrix without a real binary or I²C
/// bus. [`RealDdcutil`] is the only production implementation.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[async_trait::async_trait]
trait DdcutilOps: Send + Sync {
    /// Run `ddcutil detect --brief`, bounded by `timeout`.
    async fn detect_brief(&self, timeout: Duration) -> DdcutilOutcome;
}

/// Production [`DdcutilOps`]: `tokio::process::Command::new("ddcutil")` with
/// the fixed arguments `detect --brief` — never a shell string, and never
/// any other argument list. If a deployment's `ddcutil` build doesn't
/// support `--brief`, that surfaces as an ordinary nonzero-exit
/// [`DdcutilOutcome::Completed`], classified as an advisory disagreement by
/// [`format_second_opinion`]; it is never retried with different arguments
/// or through a shell.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RealDdcutil {
    /// Program name/path passed to `Command::new`. Production code always
    /// uses the default `"ddcutil"` (a bare name resolved via `PATH` at
    /// spawn time, unchanged from before this seam existed); tests point
    /// this at an absolute path to a scripted fake binary instead, so the
    /// timeout test never needs to mutate the process-wide `PATH` env var.
    program: std::path::PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl RealDdcutil {
    /// Production constructor: resolves `ddcutil` via `PATH` at spawn time,
    /// exactly as before this seam existed.
    fn new() -> Self {
        Self {
            program: std::path::PathBuf::from("ddcutil"),
        }
    }

    /// Test-only constructor: points at an explicit (typically absolute)
    /// path instead of doing a `PATH` lookup, so tests can hand it a
    /// scripted fake binary without mutating the process-wide `PATH`.
    #[cfg(test)]
    fn at(program: impl Into<std::path::PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[async_trait::async_trait]
impl DdcutilOps for RealDdcutil {
    async fn detect_brief(&self, timeout: Duration) -> DdcutilOutcome {
        let child = tokio::process::Command::new(&self.program)
            .args(["detect", "--brief"])
            .kill_on_drop(true)
            .output();

        match tokio::time::timeout(timeout, child).await {
            Err(_) => DdcutilOutcome::TimedOut,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => DdcutilOutcome::NotInstalled,
            Ok(Err(e)) => DdcutilOutcome::SpawnError(e.to_string()),
            Ok(Ok(output)) => DdcutilOutcome::Completed {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
        }
    }
}

/// Render a [`DdcutilOutcome`] into the one-line advisory detail appended
/// after `ddc-hi`'s own detail. Pure and independent of `ddc-hi`'s
/// pass/fail verdict — `ddcutil` never converts a usable `ddc-hi` display
/// into a hard doctor failure, and never rescues a real `ddc-hi` failure
/// either; it only gives the operator a second view of the bus so a
/// phantom display (one tool sees it, the other doesn't) is easy to spot.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn format_second_opinion(outcome: &DdcutilOutcome) -> String {
    match outcome {
        DdcutilOutcome::NotInstalled => "ddcutil: not installed".to_string(),
        DdcutilOutcome::TimedOut => "ddcutil: timed out".to_string(),
        DdcutilOutcome::SpawnError(e) => format!("ddcutil: error ({e})"),
        DdcutilOutcome::Completed {
            success,
            stdout,
            stderr,
        } => {
            let combined = format!("{stdout}{stderr}");
            let display_count = combined
                .lines()
                .filter(|l| l.trim_start().starts_with("Display "))
                .count();
            if *success && !combined.contains("Invalid display") {
                format!("ddcutil second opinion: {display_count} display(s) detected")
            } else {
                format!(
                    "ddcutil disagreement (exit {}): {display_count} display(s) detected",
                    if *success { "0" } else { "nonzero" }
                )
            }
        }
    }
}

/// Probe DDC/CI-capable displays.
///
/// Delegates to `probe_ddcci_with` with the real `ddc-hi`- and
/// `ddcutil`-backed implementations; tests inject fakes for both instead.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn probe_ddcci() -> ProbeResult {
    probe_ddcci_with(&RealVcp, &RealDdcutil::new(), DDCUTIL_TIMEOUT).await
}

/// Probe DDC/CI-capable displays via `ops`, plus an advisory `ddcutil`
/// second opinion via `ddcutil`, bounded by `ddcutil_timeout`.
///
/// The pass/fail verdict is decided from `ops` (`ddc-hi`) alone, exactly as
/// before #35; `ddcutil`'s outcome is only ever appended to the detail
/// string, never consulted for the verdict.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn probe_ddcci_with(
    ops: &impl VcpOps,
    ddcutil: &impl DdcutilOps,
    ddcutil_timeout: Duration,
) -> ProbeResult {
    let displays = ops.list_displays().await;

    let (all_ok, mut detail) = if displays.is_empty() {
        (false, "no DDC/CI displays detected".to_string())
    } else {
        // This probe is a standalone, one-shot `dormantctl doctor` diagnostic —
        // it does not share the daemon's controller chain or its process-wide
        // `PanelLocks` registry (there is no daemon-side handle to reach into
        // from here). A fresh registry, scoped to this single probe call, still
        // gives every display's read its own per-panel serialization (spec
        // §4.3) for the duration of the loop below, which is all this
        // diagnostic needs. Command priority: an operator-triggered diagnostic
        // read is command-path work, never periodic sampling.
        let panel_locks = PanelLocks::new();

        let mut details: Vec<String> = Vec::new();
        let mut all_ok = true;

        for display in &displays {
            let ident = &display.ident_string;
            let lock = panel_locks.get(ident);
            let brightness = ops.get_vcp(ident, 0x10, &lock, VcpPriority::Command).await;
            let d6 = ops.get_vcp(ident, 0xD6, &lock, VcpPriority::Command).await;

            let mut line = format!("  {ident}: brightness=");
            match brightness {
                Ok(v) => {
                    line.push_str(&v.to_string());
                }
                Err(e) => {
                    use std::fmt::Write;
                    let _ = write!(line, "ERR({e})");
                    all_ok = false;
                }
            }
            line.push_str(", power_control=");
            match d6 {
                Ok(_) => line.push_str("supported"),
                Err(_) => line.push_str("not supported"),
            }
            details.push(line);
        }

        (all_ok, details.join("\n"))
    };

    // ddcutil is advisory (see module docs): its outcome is only ever
    // appended below, never folded into `all_ok`.
    let outcome = ddcutil.detect_brief(ddcutil_timeout).await;
    let second_opinion = format_second_opinion(&outcome);
    if detail.is_empty() {
        detail = second_opinion;
    } else {
        detail.push('\n');
        detail.push_str(&second_opinion);
    }

    if all_ok {
        ProbeResult::pass("ddcci", detail)
    } else {
        ProbeResult::fail("ddcci", detail)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// RED-first scripted command-runner matrix for the `ddcutil` second opinion
// (#35). None of `DdcutilOps`, `DdcutilOutcome`, `RealDdcutil`, or
// `probe_ddcci_with` exist yet — this module is written first so the initial
// `cargo test` run fails to compile, then production code is added to make
// it pass.

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;
    use dormant_displays::vcp_ops::VcpDisplayInfo;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    fn ddcutil_is_on_path() -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };

        std::env::split_paths(&path).any(|directory| {
            std::fs::metadata(directory.join("ddcutil")).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    }

    // ── FakeVcp — scripted ddc-hi side, local to this probe's tests ────────

    /// Minimal scripted `VcpOps` fake covering exactly what `probe_ddcci_with`
    /// calls (`list_displays`, `get_vcp`). `set_vcp`/`get_vcp_raw` are never
    /// exercised by this probe, so they return an explicit "unused" error if
    /// ever called by mistake.
    struct FakeVcp {
        displays: Vec<VcpDisplayInfo>,
        responses: HashMap<(String, u8), Result<u16, String>>,
    }

    impl FakeVcp {
        /// One display, both VCP reads scripted successfully.
        fn single_ok(ident: &str, brightness: u16) -> Self {
            let mut responses = HashMap::new();
            responses.insert((ident.to_string(), 0x10), Ok(brightness));
            responses.insert((ident.to_string(), 0xD6), Ok(1));
            Self {
                displays: vec![VcpDisplayInfo {
                    ident_string: ident.to_string(),
                }],
                responses,
            }
        }

        /// One display whose brightness read fails (ddc-hi side failure).
        fn single_failing(ident: &str) -> Self {
            let mut responses = HashMap::new();
            responses.insert(
                (ident.to_string(), 0x10),
                Err("I/O error: no such device".to_string()),
            );
            responses.insert((ident.to_string(), 0xD6), Err("not supported".to_string()));
            Self {
                displays: vec![VcpDisplayInfo {
                    ident_string: ident.to_string(),
                }],
                responses,
            }
        }

        /// No displays at all (the pre-existing "no DDC/CI displays
        /// detected" fail path).
        fn none() -> Self {
            Self {
                displays: Vec::new(),
                responses: HashMap::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl VcpOps for FakeVcp {
        async fn list_displays(&self) -> Vec<VcpDisplayInfo> {
            self.displays.clone()
        }

        async fn get_vcp(
            &self,
            ident: &str,
            code: u8,
            _lock: &std::sync::Arc<dormant_displays::ddc_lock::PanelLock>,
            _prio: VcpPriority,
        ) -> Result<u16, String> {
            self.responses
                .get(&(ident.to_string(), code))
                .cloned()
                .unwrap_or_else(|| {
                    Err(format!(
                        "FakeVcp: no scripted response for {ident}/{code:#x}"
                    ))
                })
        }

        async fn set_vcp(
            &self,
            _ident: &str,
            _code: u8,
            _value: u16,
            _lock: &std::sync::Arc<dormant_displays::ddc_lock::PanelLock>,
            _prio: VcpPriority,
        ) -> Result<(), String> {
            Err("FakeVcp: set_vcp unused by probe_ddcci".to_string())
        }

        async fn get_vcp_raw(
            &self,
            _ident: &str,
            _code: u8,
            _lock: &std::sync::Arc<dormant_displays::ddc_lock::PanelLock>,
            _prio: VcpPriority,
        ) -> Result<[u8; 4], String> {
            Err("FakeVcp: get_vcp_raw unused by probe_ddcci".to_string())
        }
    }

    // ── FakeDdcutil — scripted command-runner side ─────────────────────────

    /// Scripted `DdcutilOps`: returns a fixed `DdcutilOutcome`, never spawns
    /// a real process. Lets the four command-runner branches be exercised
    /// deterministically without a `ddcutil` binary or an I²C bus.
    struct FakeDdcutil {
        outcome: DdcutilOutcome,
    }

    impl FakeDdcutil {
        fn new(outcome: DdcutilOutcome) -> Self {
            Self { outcome }
        }
    }

    #[async_trait::async_trait]
    impl DdcutilOps for FakeDdcutil {
        async fn detect_brief(&self, _timeout: Duration) -> DdcutilOutcome {
            self.outcome.clone()
        }
    }

    // ── (a) executable missing ──────────────────────────────────────────────

    #[tokio::test]
    async fn ddcutil_missing_executable_leaves_ddchi_result_unchanged() {
        let vcp = FakeVcp::single_ok("mon-1", 42);
        let ddcutil = FakeDdcutil::new(DdcutilOutcome::NotInstalled);

        let result = probe_ddcci_with(&vcp, &ddcutil, Duration::from_secs(1)).await;

        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(
            result.detail.contains("brightness=42"),
            "ddc-hi detail line missing: {}",
            result.detail
        );
        assert!(
            result.detail.contains("ddcutil: not installed"),
            "missing not-installed detail: {}",
            result.detail
        );
    }

    /// Asserts the real missing-binary branch only where `ddcutil` is absent
    /// from `PATH`; developer hosts with the optional package installed skip
    /// this environment-specific assertion while CI preserves the coverage.
    #[tokio::test]
    async fn real_ddcutil_reports_not_installed_in_this_sandbox() {
        if ddcutil_is_on_path() {
            eprintln!(
                "skipping: ddcutil present on PATH; this test only asserts the not-installed path"
            );
            return;
        }

        let outcome = RealDdcutil::new()
            .detect_brief(Duration::from_secs(5))
            .await;
        assert_eq!(outcome, DdcutilOutcome::NotInstalled);
    }

    // ── (b) exit 0 with brief detect output ─────────────────────────────────

    #[tokio::test]
    async fn ddcutil_success_appends_normalized_second_opinion() {
        let vcp = FakeVcp::single_ok("mon-1", 42);
        let ddcutil = FakeDdcutil::new(DdcutilOutcome::Completed {
            success: true,
            stdout: "Display 1\n   I2C bus:  /dev/i2c-7\n".to_string(),
            stderr: String::new(),
        });

        let result = probe_ddcci_with(&vcp, &ddcutil, Duration::from_secs(1)).await;

        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(
            result.detail.contains("second opinion") && result.detail.contains('1'),
            "expected normalized second-opinion summary: {}",
            result.detail
        );
    }

    // ── (c) exit nonzero / "Invalid display" ────────────────────────────────

    #[tokio::test]
    async fn ddcutil_disagreement_does_not_flip_a_passing_ddchi_result() {
        let vcp = FakeVcp::single_ok("mon-1", 42);
        let ddcutil = FakeDdcutil::new(DdcutilOutcome::Completed {
            success: false,
            stdout: String::new(),
            stderr: "ddcutil: Invalid display".to_string(),
        });

        let result = probe_ddcci_with(&vcp, &ddcutil, Duration::from_secs(1)).await;

        assert_eq!(
            result.status,
            ProbeStatus::Pass,
            "ddcutil disagreement must not fail a usable ddc-hi display: {result:?}"
        );
        assert!(
            result.detail.to_lowercase().contains("disagreement"),
            "expected disagreement detail: {}",
            result.detail
        );
    }

    #[tokio::test]
    async fn ddcutil_agreement_does_not_rescue_a_failing_ddchi_result() {
        let vcp = FakeVcp::single_failing("mon-1");
        let ddcutil = FakeDdcutil::new(DdcutilOutcome::Completed {
            success: true,
            stdout: "Display 1\n   I2C bus:  /dev/i2c-7\n".to_string(),
            stderr: String::new(),
        });

        let result = probe_ddcci_with(&vcp, &ddcutil, Duration::from_secs(1)).await;

        assert_eq!(
            result.status,
            ProbeStatus::Fail,
            "a ddcutil success must not mask a real ddc-hi read failure: {result:?}"
        );
        assert!(result.detail.contains("second opinion"));
    }

    #[tokio::test]
    async fn ddcutil_disagreement_surfaces_phantom_bus_when_ddchi_finds_nothing() {
        let vcp = FakeVcp::none();
        let ddcutil = FakeDdcutil::new(DdcutilOutcome::Completed {
            success: true,
            stdout: "Display 1\n   I2C bus:  /dev/i2c-7\n".to_string(),
            stderr: String::new(),
        });

        let result = probe_ddcci_with(&vcp, &ddcutil, Duration::from_secs(1)).await;

        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("no DDC/CI displays detected"));
        assert!(result.detail.contains("second opinion"));
    }

    // ── (d) timeout ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ddcutil_timeout_appends_timed_out_and_never_hangs() {
        let vcp = FakeVcp::single_ok("mon-1", 42);
        let ddcutil = FakeDdcutil::new(DdcutilOutcome::TimedOut);

        // Outer canary timeout: if `probe_ddcci_with` ever blocked on the
        // second opinion instead of respecting the bounded budget, this
        // test would hang rather than fail cleanly — 2s is generous slack
        // over the (fake, instant) inner outcome.
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            probe_ddcci_with(&vcp, &ddcutil, Duration::from_millis(50)),
        )
        .await
        .expect("probe_ddcci_with must not hang past its own bounded timeout");

        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(
            result.detail.contains("ddcutil: timed out"),
            "missing timed-out detail: {}",
            result.detail
        );
    }

    /// Proves the *real* bounded timeout wrapping in `RealDdcutil` actually
    /// enforces the budget, not just that the `TimedOut` enum variant
    /// threads through when a fake hands it back. Uses a slow real child
    /// process (`sleep`) so the real `tokio::time::timeout` around
    /// `Command::output()` is exercised end to end.
    #[tokio::test]
    async fn real_ddcutil_enforces_bounded_timeout_against_a_slow_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("ddcutil");
        std::fs::write(&script_path, "#!/bin/sh\nsleep 10\n").expect("write fake ddcutil");
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        // Point `RealDdcutil` straight at the scripted fake binary's
        // absolute path instead of mutating the process-wide `PATH` env
        // var: tests run as threads within one process (not separate
        // processes), so a `PATH` mutation here would race the sibling
        // `real_ddcutil_reports_not_installed_in_this_sandbox` test's
        // concurrent PATH-dependent spawn.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            RealDdcutil::at(&script_path).detect_brief(Duration::from_millis(100)),
        )
        .await
        .expect("RealDdcutil.detect_brief must return within its own bounded timeout");

        assert_eq!(outcome, DdcutilOutcome::TimedOut);
    }
}
