//! macOS power-assertion doctor probe — reports active display-sleep-
//! preventing power assertions, and fails when any of them is persistently
//! owned by dormant itself.
//!
//! Read-only: shells out to `pmset -g assertions` (a read-only inventory
//! command; it never declares or releases anything) — never touches
//! `IOPMAssertionDeclareUserActivity`/`IOPMAssertionRelease`, which are
//! `dormant_displays::macos_power::RealDisplaySleepTransport`'s job during
//! an actual wake, not this diagnostic's. Mirrors
//! `crate::probes::ddcci`'s `DdcutilOps` command-runner seam (bounded
//! timeout, `NotFound` → treated as "can't enumerate" rather than a hard
//! failure, scripted fakes for every branch).
//!
//! ## Why this can fail
//!
//! `dormant_displays::macos_display_sleep::MacosDisplaySleepController::wake`
//! declares a short-lived `IOPMAssertionDeclareUserActivity` assertion
//! (name: `"dormant wake confirmation"` —
//! `dormant_displays::macos_power::RealDisplaySleepTransport::declare_user_activity`)
//! and releases it via RAII (`AssertionGuard::drop`) the moment `wake()`
//! returns — see that module's docs. If this probe ever finds a
//! dormant-owned, display-sleep-preventing assertion still active (by
//! process name or by the exact assertion name string), that is a bug: the
//! RAII guard failed to release, or a wake is stuck forever mid-flight —
//! either way silently keeping the whole Mac awake. That is this probe's
//! one and only `Fail` condition; any other outstanding assertion (browser,
//! video call, another app) is none of dormant's business and is only ever
//! reported, never failed on.

// The platform-neutral logic below (the `pmset` command-runner seam,
// `parse_display_sleep_assertions`, `probe_macos_power_with`) is only ever
// reached in production from the `#[cfg(target_os = "macos")]`-gated
// `probe_macos_power` at the bottom of this file — on a non-macOS,
// non-test build it is genuinely unreachable. Mirrors the identical
// situation (and identical fix) in `dormantd::macos_idle`'s own
// `macos_run`.
#![cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]

use std::time::Duration;

use crate::types::ProbeResult;

/// Bounded budget for the `pmset -g assertions` inventory call — mirrors
/// `crate::probes::ddcci`'s `DDCUTIL_TIMEOUT` reasoning: a hung/rogue
/// `pmset` must never stall the doctor.
const PMSET_ASSERTIONS_TIMEOUT: Duration = Duration::from_secs(5);

/// Assertion type names this probe treats as "prevents display sleep" —
/// matched by substring against `pmset -g assertions`'s "Listed by owning
/// process" lines (macOS's own assertion-type identifiers).
const DISPLAY_SLEEP_ASSERTION_MARKERS: &[&str] = &[
    "PreventUserIdleDisplaySleep",
    "PreventDisplaySleep",
    "NoDisplaySleepAssertion",
];

/// Outcome of one bounded `pmset -g assertions` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PmsetOutcome {
    /// `pmset` is not on `PATH`. Should never happen on real macOS (it
    /// ships with the OS) — handled the same defensive way
    /// `crate::probes::ddcci`'s `ddcutil` seam handles its own missing
    /// binary.
    NotInstalled,
    /// The command ran to completion (any exit status) within the timeout.
    Completed {
        success: bool,
        stdout: String,
        stderr: String,
    },
    /// The command did not finish within [`PMSET_ASSERTIONS_TIMEOUT`].
    TimedOut,
    /// Some other spawn/IO failure.
    SpawnError(String),
}

/// Seam over invoking `pmset -g assertions` as an external process, so
/// tests can script every branch without a real `pmset` binary. The only
/// method on this trait is a READ (an inventory listing) — there is no
/// mutating counterpart on this seam at all, so this probe's read-only
/// invariant holds by construction, not just by convention.
#[async_trait::async_trait]
trait PmsetAssertionsOps: Send + Sync {
    /// Run `pmset -g assertions`, bounded by `timeout`.
    async fn get_assertions(&self, timeout: Duration) -> PmsetOutcome;
}

/// Production [`PmsetAssertionsOps`]: `tokio::process::Command::new("pmset")`
/// with the fixed arguments `-g assertions` — never a shell string.
///
/// Unlike the rest of this file's platform-neutral logic, `RealPmset` is
/// used ONLY by the `#[cfg(target_os = "macos")]`-gated `probe_macos_power`
/// entry point below — no test in this module exercises it (tests script
/// [`PmsetAssertionsOps`] via `FakePmset` instead), so it is dead code on
/// every non-macOS build, test builds included; the file-level
/// `not(any(test, target_os = "macos"))` attribute above does not cover
/// that case, hence this narrower one.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct RealPmset {
    /// Program name/path passed to `Command::new`. Production code always
    /// uses the default `"pmset"` (`PATH`-resolved at spawn time); tests
    /// point this at an absolute path to a scripted fake binary instead.
    program: std::path::PathBuf,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl RealPmset {
    fn new() -> Self {
        Self {
            program: std::path::PathBuf::from("pmset"),
        }
    }
}

#[async_trait::async_trait]
impl PmsetAssertionsOps for RealPmset {
    async fn get_assertions(&self, timeout: Duration) -> PmsetOutcome {
        let child = tokio::process::Command::new(&self.program)
            .args(["-g", "assertions"])
            .kill_on_drop(true)
            .output();

        match tokio::time::timeout(timeout, child).await {
            Err(_) => PmsetOutcome::TimedOut,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => PmsetOutcome::NotInstalled,
            Ok(Err(e)) => PmsetOutcome::SpawnError(e.to_string()),
            Ok(Ok(output)) => PmsetOutcome::Completed {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
        }
    }
}

/// One display-sleep-preventing assertion line found in `pmset -g
/// assertions`'s "Listed by owning process" section.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AssertionLine {
    raw: String,
    owned_by_dormant: bool,
}

/// Parse `pmset -g assertions`' stdout for per-owner lines naming a
/// display-sleep-preventing assertion type.
///
/// Deliberately conservative: only lines that look like a "Listed by owning
/// process" row (contain both a parenthesised process name and a `:`
/// separator before the assertion type) are considered — the leading
/// "Assertion status system-wide:" summary section repeats the same marker
/// words with a bare per-type count and is never a per-owner assertion, so
/// it is skipped.
fn parse_display_sleep_assertions(stdout: &str) -> Vec<AssertionLine> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !DISPLAY_SLEEP_ASSERTION_MARKERS
            .iter()
            .any(|m| trimmed.contains(m))
        {
            continue;
        }
        if !trimmed.contains('(') || !trimmed.contains(':') {
            continue;
        }
        let lower = trimmed.to_lowercase();
        // Matches both a "dormant"-named process (e.g. `pid NNN(dormantd)`)
        // and the literal assertion name string dormant declares
        // (`"dormant wake confirmation"` — see the module docs).
        let owned_by_dormant = lower.contains("dormant");
        out.push(AssertionLine {
            raw: trimmed.to_string(),
            owned_by_dormant,
        });
    }
    out
}

/// Probe active macOS power assertions via `ops`.
async fn probe_macos_power_with(ops: &impl PmsetAssertionsOps, timeout: Duration) -> ProbeResult {
    match ops.get_assertions(timeout).await {
        PmsetOutcome::NotInstalled => ProbeResult::skip(
            "macos-power",
            "pmset not found on PATH — cannot enumerate power assertions",
        ),
        PmsetOutcome::TimedOut => ProbeResult::fail(
            "macos-power",
            format!("pmset -g assertions timed out after {timeout:?}"),
        ),
        PmsetOutcome::SpawnError(e) => {
            ProbeResult::fail("macos-power", format!("failed to spawn pmset: {e}"))
        }
        PmsetOutcome::Completed {
            success: false,
            stdout,
            stderr,
        } => ProbeResult::fail(
            "macos-power",
            format!("pmset -g assertions exited non-zero; stdout: {stdout}; stderr: {stderr}"),
        ),
        PmsetOutcome::Completed {
            success: true,
            stdout,
            ..
        } => {
            let assertions = parse_display_sleep_assertions(&stdout);
            let dormant_owned: Vec<&AssertionLine> =
                assertions.iter().filter(|a| a.owned_by_dormant).collect();
            if dormant_owned.is_empty() {
                ProbeResult::pass(
                    "macos-power",
                    format!(
                        "{} display-sleep-preventing assertion(s) active, none owned by dormant",
                        assertions.len()
                    ),
                )
            } else {
                let detail = dormant_owned
                    .iter()
                    .map(|a| a.raw.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                ProbeResult::fail(
                    "macos-power",
                    format!(
                        "{} dormant-owned display-sleep assertion(s) still active — the RAII \
                         release (AssertionGuard::drop) should have cleared these; a stuck \
                         assertion silently prevents the Mac from ever sleeping:\n{detail}",
                        dormant_owned.len()
                    ),
                )
            }
        }
    }
}

/// Probe the real macOS power-assertion inventory. Only available on
/// macOS.
#[cfg(target_os = "macos")]
pub async fn probe_macos_power() -> ProbeResult {
    probe_macos_power_with(&RealPmset::new(), PMSET_ASSERTIONS_TIMEOUT).await
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeStatus;

    struct FakePmset {
        outcome: PmsetOutcome,
    }

    impl FakePmset {
        fn new(outcome: PmsetOutcome) -> Self {
            Self { outcome }
        }
    }

    #[async_trait::async_trait]
    impl PmsetAssertionsOps for FakePmset {
        async fn get_assertions(&self, _timeout: Duration) -> PmsetOutcome {
            self.outcome.clone()
        }
    }

    const NO_DORMANT_ASSERTIONS: &str = "\
Assertion status system-wide:
   PreventUserIdleDisplaySleep    1
   PreventUserIdleSystemSleep     0
Listed by owning process:
   pid 92(WindowServer): [0x000000010000000a] 00:03:37 PreventUserIdleDisplaySleep named: \"com.apple.iohideventsystem\"
";

    const DORMANT_OWNED_ASSERTION: &str = "\
Assertion status system-wide:
   PreventUserIdleDisplaySleep    1
Listed by owning process:
   pid 205(dormantd): [0x0000000200000123] 00:14:02 PreventUserIdleDisplaySleep named: \"dormant wake confirmation\"
";

    #[tokio::test]
    async fn pmset_not_installed_is_skip() {
        let ops = FakePmset::new(PmsetOutcome::NotInstalled);
        let result = probe_macos_power_with(&ops, Duration::from_secs(1)).await;
        assert_eq!(result.status, ProbeStatus::Skip, "{result:?}");
    }

    #[tokio::test]
    async fn pmset_timeout_is_fail() {
        let ops = FakePmset::new(PmsetOutcome::TimedOut);
        let result = probe_macos_power_with(&ops, Duration::from_millis(50)).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("timed out"));
    }

    #[tokio::test]
    async fn no_dormant_assertions_is_pass() {
        let ops = FakePmset::new(PmsetOutcome::Completed {
            success: true,
            stdout: NO_DORMANT_ASSERTIONS.to_string(),
            stderr: String::new(),
        });
        let result = probe_macos_power_with(&ops, PMSET_ASSERTIONS_TIMEOUT).await;
        assert_eq!(result.status, ProbeStatus::Pass, "{result:?}");
        assert!(result.detail.contains("none owned by dormant"));
    }

    #[tokio::test]
    async fn persistent_dormant_assertion_is_fail() {
        let ops = FakePmset::new(PmsetOutcome::Completed {
            success: true,
            stdout: DORMANT_OWNED_ASSERTION.to_string(),
            stderr: String::new(),
        });
        let result = probe_macos_power_with(&ops, Duration::from_secs(1)).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("dormant wake confirmation"));
        assert!(result.detail.contains("dormant-owned"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_fail() {
        let ops = FakePmset::new(PmsetOutcome::Completed {
            success: false,
            stdout: String::new(),
            stderr: "permission denied".to_string(),
        });
        let result = probe_macos_power_with(&ops, Duration::from_secs(1)).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("permission denied"));
    }

    #[tokio::test]
    async fn spawn_error_is_fail() {
        let ops = FakePmset::new(PmsetOutcome::SpawnError(
            "simulated spawn error".to_string(),
        ));
        let result = probe_macos_power_with(&ops, Duration::from_secs(1)).await;
        assert_eq!(result.status, ProbeStatus::Fail, "{result:?}");
        assert!(result.detail.contains("simulated spawn error"));
    }

    #[test]
    fn parser_skips_the_system_wide_summary_section() {
        let assertions = parse_display_sleep_assertions(NO_DORMANT_ASSERTIONS);
        // Exactly one per-owner line, not the summary's bare-count line.
        assert_eq!(assertions.len(), 1, "{assertions:?}");
        assert!(!assertions[0].owned_by_dormant);
    }
}
