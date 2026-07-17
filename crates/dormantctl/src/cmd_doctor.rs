//! `dormantctl doctor` — hardware/connectivity verification.
//!
//! Runs probes against configured sensors, displays, and credentials to
//! diagnose connectivity and capability issues without needing a running
//! daemon.  Probe logic is delegated to the `dormant-doctor` crate; this
//! module handles CLI argument parsing, config loading, table rendering, and
//! exit-code mapping.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use comfy_table::Color;
use comfy_table::ContentArrangement;
use comfy_table::{Cell, Row, Table};
use dormant_core::config::schema::Credentials;
use dormant_core::config::{Strictness, load_config, load_credentials};
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::paths;
use dormant_core::rules::{ExerciseReport, ExerciseStep, ExerciseVerdict};
use dormant_doctor::{ProbeResult, ProbeStatus};

use dormantctl::client;

// ── DoctorOutcome ───────────────────────────────────────────────────────────────

/// The outcome of a `doctor` invocation: exit code + optional message.
#[derive(Debug, Clone, PartialEq)]
pub enum DoctorOutcome {
    /// All probes passed or were skipped.
    AllOk,
    /// At least one probe failed.
    SomeFailed,
    /// The subcommand is not yet supported (exit 3).
    NotSupported(String),
}

// ── CLI ─────────────────────────────────────────────────────────────────────────

/// Diagnose hardware and connectivity.
#[derive(Parser, Debug)]
pub struct DoctorArgs {
    /// Path to the config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Path to the credentials file.
    #[arg(long)]
    pub credentials: Option<PathBuf>,

    #[command(subcommand)]
    pub subcommand: Option<DoctorSubcommand>,
}

#[derive(clap::Subcommand, Debug)]
pub enum DoctorSubcommand {
    /// Probe DDC/CI displays.
    Ddcci,
    /// Probe a USB LD2410 radar sensor.
    Usb {
        /// Serial port path (e.g. `/dev/ttyUSB0`).
        port: String,
        /// Baud rate (default 256000).
        #[arg(long, default_value = "256000")]
        baud: u32,
    },
    /// Probe MQTT sensors.
    Mqtt,
    /// Probe Home Assistant WebSocket sensors.
    Ha,
    /// Validate configuration.
    Config,
    /// Probe `KWin` DPMS (not yet supported).
    Kwin,
    /// Probe the macOS idle clock (two bounded raw readings, `Fail` when
    /// they are identical). macOS only.
    MacosIdle,
    /// Probe macOS display-sleep API availability and current per-display
    /// asleep/awake state. Read-only — never blanks or wakes a display.
    /// macOS only.
    MacosDisplaySleep,
    /// Probe active macOS power assertions preventing display sleep.
    /// `Fail` when a dormant-owned assertion is still active. macOS only.
    MacosPower,
    /// Probe Samsung Tizen displays (reachability, power state, token).
    Samsung,
    /// Control-path verification: blank → read → wake → read → restore a
    /// single display and report whether each step demonstrably moved the
    /// panel.
    ///
    /// Routes through the daemon (Option B per the design doc). The
    /// daemon pauses the target's rule(s) for the exercise window, runs
    /// the sequence on its live controllers, and replies with a per-step
    /// report. Exit code is non-zero only when at least one step verdict
    /// is `Failed` — a confirmable panel that did not move despite the
    /// command returning `Ok`.
    Exercise {
        /// Display id to exercise (must match a `[displays.<id>]` key in
        /// the daemon's loaded config).
        display: String,
    },
}

// ── Run ─────────────────────────────────────────────────────────────────────────

/// Run the `doctor` command.
///
/// # Errors
///
/// Propagates I/O and config-loading errors.
pub fn run(args: &DoctorArgs) -> Result<DoctorOutcome> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(args))
}

async fn run_async(args: &DoctorArgs) -> Result<DoctorOutcome> {
    match &args.subcommand {
        Some(DoctorSubcommand::Ddcci) => {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let results = vec![dormant_doctor::probe_ddcci().await];
                print_table(&results);
                Ok(outcome(&results))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                Ok(DoctorOutcome::NotSupported("ddcci".into()))
            }
        }
        Some(DoctorSubcommand::Usb { port, baud }) => {
            let results = vec![dormant_doctor::probe_usb(port, *baud).await];
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Mqtt) => {
            let (cfg, creds, note) = load_config_and_creds(args)?;
            if let Some(n) = &note {
                println!("{n}");
            }
            let results = dormant_doctor::probe_mqtt_all(&cfg, &creds).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Ha) => {
            let (cfg, creds, note) = load_config_and_creds(args)?;
            if let Some(n) = &note {
                println!("{n}");
            }
            let results = dormant_doctor::probe_ha_all(&cfg, &creds).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Config) => {
            let (results, note) = probe_config(args);
            if let Some(n) = &note {
                println!("{n}");
            }
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Kwin) => Ok(DoctorOutcome::NotSupported("kwin-dpms".into())),
        Some(DoctorSubcommand::MacosIdle) => run_macos_idle().await,
        Some(DoctorSubcommand::MacosDisplaySleep) => run_macos_display_sleep().await,
        Some(DoctorSubcommand::MacosPower) => run_macos_power().await,
        Some(DoctorSubcommand::Samsung) => {
            let (cfg, creds, note) = load_config_and_creds(args)?;
            if let Some(n) = &note {
                println!("{n}");
            }
            let results = dormant_doctor::probe_samsung(&cfg, &creds).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Exercise { display: _ }) => {
            // The doctor's CLI shape doesn't surface `--socket` (that's a
            // global on the parent `dormantctl` parser), so the exercise
            // subcommand is handled in `main.rs` after the socket path is
            // resolved.  Any path that reaches here means the parser was
            // wired up wrong.
            unreachable!("doctor exercise is dispatched by main.rs with the resolved socket path")
        }
        None => {
            // Bare doctor: delegate to the single-source orchestration.
            let (cfg, creds, note) = load_config_and_creds(args)?;
            if let Some(n) = &note {
                println!("{n}");
            }
            let results = dormant_doctor::probe_all_offline(&cfg, &creds).await;
            print_table(&results);
            Ok(outcome(&results))
        }
    }
}

// ── macOS read-only doctor arms (Task 11) ─────────────────────────────────────────
//
// Extracted out of `run_async` (each one inlined there would push it over
// `clippy::too_many_lines`) — mirrors the `Ddcci`/`Kwin` NotSupported-off-
// platform pattern already used above, just as its own named function per
// arm since there are three of them.

/// `doctor macos-idle` — two bounded raw readings of the idle clock.
///
/// `async` only actually awaits anything on macOS (`#[cfg(not(target_os =
/// "macos"))]`'s body is a bare `Ok(..)`) — the `unused_async` lint fires on
/// every non-macOS build, so it is suppressed there specifically rather
/// than dropping `async` (which would require a matching, more invasive
/// signature change at the `Some(DoctorSubcommand::MacosIdle) => ...await`
/// call site above, needed on macOS).
#[cfg_attr(not(target_os = "macos"), allow(clippy::unused_async))]
async fn run_macos_idle() -> Result<DoctorOutcome> {
    #[cfg(target_os = "macos")]
    {
        let results = vec![dormant_doctor::probe_macos_idle().await];
        print_table(&results);
        Ok(outcome(&results))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(DoctorOutcome::NotSupported("macos-idle".into()))
    }
}

/// `doctor macos-display-sleep` — API availability + current per-display
/// asleep/awake state. Read-only.
#[cfg_attr(not(target_os = "macos"), allow(clippy::unused_async))]
async fn run_macos_display_sleep() -> Result<DoctorOutcome> {
    #[cfg(target_os = "macos")]
    {
        let results = vec![dormant_doctor::probe_macos_display_sleep().await];
        print_table(&results);
        Ok(outcome(&results))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(DoctorOutcome::NotSupported("macos-display-sleep".into()))
    }
}

/// `doctor macos-power` — active display-sleep-preventing power assertions.
#[cfg_attr(not(target_os = "macos"), allow(clippy::unused_async))]
async fn run_macos_power() -> Result<DoctorOutcome> {
    #[cfg(target_os = "macos")]
    {
        let results = vec![dormant_doctor::probe_macos_power().await];
        print_table(&results);
        Ok(outcome(&results))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(DoctorOutcome::NotSupported("macos-power".into()))
    }
}

// ── Config loading ──────────────────────────────────────────────────────────────

/// Load config and credentials using the same default-path logic as `validate`.
///
/// Credential errors are degraded gracefully: missing, unreadable, or
/// invalid credential files produce an empty `Credentials` plus a note
/// so every other probe still executes.  The note is printed by the
/// caller before the probe table.
fn load_config_and_creds(
    args: &DoctorArgs,
) -> Result<(dormant_core::config::Config, Credentials, Option<String>)> {
    let config_path =
        paths::resolve_config_path(args.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    let (cfg, _warnings) = load_config(&config_path, Strictness::Warn)?;
    let (creds, note) = load_credentials_resilient(&creds_path);
    Ok((cfg, creds, note))
}

/// Load credentials with diagnosis-preserving degradation.
///
/// - **Missing file** → proceed anonymous; note says "no file at `<path>`".
/// - **Unreadable / invalid TOML / wrong perms** → proceed anonymous; note
///   carries the actual error text so the operator can fix it.
/// - **Readable file** → normal credentials load.
///
/// This keeps the doctor usable even when the credentials file has a
/// problem — the probe can still detect `NotAuthorized` and surface the
/// correct root cause.
fn load_credentials_resilient(path: &std::path::Path) -> (Credentials, Option<String>) {
    if !path.exists() {
        return (
            Credentials::default(),
            Some(format!(
                "credentials: no file at {} — auth-dependent probes run anonymous",
                path.display()
            )),
        );
    }

    match load_credentials(path) {
        Ok(creds) => (creds, None),
        Err(e) => (
            Credentials::default(),
            Some(format!(
                "credentials: {e} — auth-dependent probes run anonymous"
            )),
        ),
    }
}

// ── Probe: config (CLI wrapper) ─────────────────────────────────────────────────

/// CLI-level config probe: loads config + credentials, then delegates to the
/// doctor crate.
///
/// Returns the probe results and any credential-loading note that should be
/// printed before the table.
fn probe_config(args: &DoctorArgs) -> (Vec<ProbeResult>, Option<String>) {
    match load_config_and_creds(args) {
        Ok((cfg, creds, note)) => (vec![dormant_doctor::probe_config_inner(&cfg, &creds)], note),
        Err(e) => (vec![ProbeResult::fail("config", format!("{e:#}"))], None),
    }
}

// ── Table printing ──────────────────────────────────────────────────────────────

/// Render probe results as a table, returning the rendered text.
///
/// Extracted from `print_table` so it can be exercised directly in tests
/// without capturing stdout.
fn format_table(results: &[ProbeResult]) -> String {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Probe", "Status", "Detail"]);

    for r in results {
        let (glyph, color) = match r.status {
            ProbeStatus::Pass => ("✓", Color::Green),
            ProbeStatus::Fail => ("✗", Color::Red),
            ProbeStatus::Skip => ("-", Color::Yellow),
            ProbeStatus::NotSupported => ("N/A", Color::Yellow),
        };
        table.add_row(Row::from(vec![
            Cell::new(&r.name),
            Cell::new(glyph).fg(color),
            Cell::new(&r.detail),
        ]));
    }

    table.to_string()
}

/// Print a table of probe results.
fn print_table(results: &[ProbeResult]) {
    println!("{}", format_table(results));
}

/// Determine the overall outcome from probe results.
fn outcome(results: &[ProbeResult]) -> DoctorOutcome {
    if results.iter().any(|r| r.status == ProbeStatus::Fail) {
        DoctorOutcome::SomeFailed
    } else {
        DoctorOutcome::AllOk
    }
}

// ── Exercise subcommand (control-path verification) ───────────────────────────

/// Run `dormantctl doctor --exercise <display>` — IPC-only in v1 (per the
/// design doc's Option B).  Sends `IpcRequest::Exercise` to the daemon and
/// prints the per-step report.
///
/// Exit semantics:
/// - `SomeFailed` (CLI exits 1) when ANY step's verdict is `Failed` —
///   a confirmable panel that did not move despite the command returning
///   `Ok`.  This is the systemic guard against the samsung stale-socket /
///   port-1516 400s failure shape.
/// - `AllOk` (CLI exits 0) for any combination of `Confirmed` and
///   `Unconfirmable` — a `Confirmed` panel moved as expected, an
///   `Unconfirmable` panel has no readback (command / kwin-dpms /
///   ha-passthrough controllers) so the test honestly says "issued, can't
///   observe" rather than fabricating a pass.
pub fn run_exercise_with_socket(
    socket_path: &std::path::Path,
    display: &str,
) -> Result<DoctorOutcome> {
    let resp = client::send_request(
        socket_path,
        &IpcRequest::Exercise {
            display: display.to_string(),
        },
    )?;
    if !resp.ok {
        eprintln!(
            "error: exercise failed: {}",
            resp.error.as_deref().unwrap_or("unknown")
        );
        return Ok(DoctorOutcome::SomeFailed);
    }
    match resp.exercise_report {
        Some(report) => Ok(present_exercise_report(&report)),
        None => Ok(report_no_exercise()),
    }
}

/// Print the per-step exercise report and return the doctor outcome.
///
/// Glyph mapping (consistent with the rest of the CLI; mirrors the
/// `print_table` glyphs so an operator reading `doctor` output sees the
/// same vocabulary):
/// - ✓ Confirmed (green)
/// - ~ Unconfirmable (yellow)
/// - ✗ Failed (red)
fn present_exercise_report(report: &ExerciseReport) -> DoctorOutcome {
    println!(
        "exercise: display={} pre_phase={} paused_rules={}",
        report.display,
        report.pre_phase,
        if report.paused_rules.is_empty() {
            "none".to_string()
        } else {
            report
                .paused_rules
                .iter()
                .map(|r| r.0.as_str())
                .collect::<Vec<_>>()
                .join(",")
        },
    );
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Step", "Verdict", "Detail"]);
    let mut any_failed = false;
    for step in &report.steps {
        let (glyph, color) = match step.verdict {
            ExerciseVerdict::Confirmed => ("✓", Color::Green),
            ExerciseVerdict::Unconfirmable => ("~", Color::Yellow),
            ExerciseVerdict::Failed => {
                any_failed = true;
                ("✗", Color::Red)
            }
        };
        let detail = step_detail(step);
        table.add_row(Row::from(vec![
            Cell::new(&step.command).fg(color),
            Cell::new(glyph).fg(color),
            Cell::new(detail),
        ]));
    }
    println!("{table}");
    if any_failed {
        DoctorOutcome::SomeFailed
    } else {
        DoctorOutcome::AllOk
    }
}

/// Emit the operator-facing diagnostic when the daemon returned
/// `ok: true` with no `exercise_report` — the wire shape implies a daemon
/// regression (the response field is `serde(default)` + `skip_serializing_if`,
/// so a missing field means the daemon's `IpcResponse` serialisation broke).
fn report_no_exercise() -> DoctorOutcome {
    eprintln!("error: daemon returned ok but no exercise_report");
    DoctorOutcome::SomeFailed
}

/// Compose a one-line description of a step for the report table.
fn step_detail(step: &ExerciseStep) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(mode) = step.blank_mode {
        parts.push(format!("mode={mode:?}"));
    }
    parts.push(format!("cmd_ok={}", step.returned_ok));
    if let Some(before) = &step.state_before {
        parts.push(format!("before={before:?}"));
    }
    if let Some(after) = &step.state_after {
        parts.push(format!("after={after:?}"));
    }
    if let Some(err) = &step.error {
        parts.push(format!("err={err}"));
    }
    parts.join(" ")
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Table formatting ────────────────────────────────────────────────────

    #[test]
    fn format_table_contains_glyphs_and_names() {
        let results = vec![
            ProbeResult::pass("test-pass", "all good"),
            ProbeResult::fail("test-fail", "something broke"),
            ProbeResult::skip("test-skip", "not applicable"),
            ProbeResult::not_supported("test-na", "not on this platform"),
            ProbeResult::fail("test-multiline", "line one\nline two"),
        ];

        let output = format_table(&results);

        assert!(output.contains('✓'), "table should contain checkmark");
        assert!(output.contains('✗'), "table should contain X mark");
        assert!(output.contains('-'), "table should contain skip dash");
        assert!(
            output.contains("N/A"),
            "table should contain NotSupported glyph"
        );
        assert!(
            output.contains("test-pass"),
            "table should contain probe name"
        );
        assert!(
            output.contains("test-fail"),
            "table should contain probe name"
        );
        assert!(
            output.contains("test-skip"),
            "table should contain probe name"
        );
        assert!(
            output.contains("test-na"),
            "table should contain probe name"
        );
        assert!(
            output.contains("all good"),
            "table should contain probe detail"
        );
        assert!(
            output.contains("line one"),
            "table should contain first line of multiline detail"
        );
        assert!(
            output.contains("line two"),
            "table should contain second line of multiline detail"
        );
    }

    // ── DoctorOutcome ───────────────────────────────────────────────────────

    #[test]
    fn outcome_all_pass_returns_all_ok() {
        let results = [
            ProbeResult::pass("a", ""),
            ProbeResult::skip("b", ""),
            ProbeResult::not_supported("c", ""),
        ];
        assert_eq!(outcome(&results), DoctorOutcome::AllOk);
    }

    #[test]
    fn outcome_any_fail_returns_some_failed() {
        let results = [ProbeResult::pass("a", ""), ProbeResult::fail("b", "broken")];
        assert_eq!(outcome(&results), DoctorOutcome::SomeFailed);
    }

    #[test]
    fn outcome_all_skip_returns_all_ok() {
        let results = [ProbeResult::skip("a", "no config")];
        assert_eq!(outcome(&results), DoctorOutcome::AllOk);
    }

    // ── load_credentials_resilient ────────────────────────────────────────────

    fn make_temp_path(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let name = format!("dormantctl-test-{prefix}-{}", std::process::id());
        dir.join(name)
    }

    #[test]
    fn load_credentials_resilient_missing_file() {
        let path = make_temp_path("missing");
        // Ensure it does not exist.
        let _ = std::fs::remove_file(&path);

        let (creds, note) = load_credentials_resilient(&path);

        assert_eq!(creds, Credentials::default());
        assert!(note.is_some(), "missing file should produce a note");
        let n = note.unwrap();
        assert!(
            n.contains("credentials: no file at"),
            "note should mention missing file; got: {n}"
        );
        assert!(
            n.contains("auth-dependent probes run anonymous"),
            "note should say probes run anonymous; got: {n}"
        );
    }

    #[test]
    fn load_credentials_resilient_perms_error() {
        let path = make_temp_path("perms");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"ha_token = \"fake\"\n").unwrap();

        // Set permissions to 0o644 (world-readable) — should trigger
        // CredsPerms on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
            // Sanity: mode is not 0o600.
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_ne!(
                mode & 0o777,
                0o600,
                "test setup failed: expected non-600 mode"
            );
        }

        let (creds, note) = load_credentials_resilient(&path);

        assert_eq!(creds, Credentials::default());
        assert!(note.is_some(), "perms error should produce a note");
        let n = note.unwrap();
        assert!(
            n.contains("E_CREDS_PERMS"),
            "note should contain E_CREDS_PERMS; got: {n}"
        );
        assert!(
            n.contains("auth-dependent probes run anonymous"),
            "note should say probes run anonymous; got: {n}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_credentials_resilient_invalid_toml() {
        let path = make_temp_path("invalid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"this is not toml\n").unwrap();

        // Set permissions to 0o600 (passes the perms gate).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let (creds, note) = load_credentials_resilient(&path);

        assert_eq!(creds, Credentials::default());
        assert!(note.is_some(), "invalid TOML should produce a note");
        let n = note.unwrap();
        assert!(
            n.contains("E_CONFIG_INVALID"),
            "note should contain E_CONFIG_INVALID; got: {n}"
        );
        assert!(
            n.contains("auth-dependent probes run anonymous"),
            "note should say probes run anonymous; got: {n}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── Exercise report rendering ──────────────────────────────────────────

    /// Decisive test: a report with one `Failed` step must produce
    /// `SomeFailed` (CLI exits 1) — the operator-facing signal that the
    /// panel did not move despite the controller reporting `Ok`.  The
    /// other two steps are `Confirmed` and `Unconfirmable` to confirm
    /// mixed reports still surface the failure.
    #[test]
    fn present_exercise_report_with_failed_step_returns_some_failed() {
        use dormant_core::types::{BlankMode, DisplayId, RuleId};

        let report = ExerciseReport {
            display: DisplayId("mon".into()),
            pre_phase: "active".into(),
            paused_rules: vec![RuleId("office".into())],
            steps: vec![
                ExerciseStep {
                    command: "read".into(),
                    blank_mode: None,
                    returned_ok: true,
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Unconfirmable,
                    error: None,
                },
                ExerciseStep {
                    command: "blank".into(),
                    blank_mode: Some(BlankMode::PowerOff),
                    returned_ok: true, // controller lied
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Failed, // panel didn't move
                    error: None,
                },
                ExerciseStep {
                    command: "wake".into(),
                    blank_mode: None,
                    returned_ok: true,
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Confirmed,
                    error: None,
                },
            ],
        };
        let outcome = present_exercise_report(&report);
        assert_eq!(
            outcome,
            DoctorOutcome::SomeFailed,
            "a Failed step must produce SomeFailed (CLI exit 1)"
        );
    }

    /// Counterpart: a report with no `Failed` steps returns `AllOk`
    /// regardless of how many `Unconfirmable` rows appear.  This is the
    /// honest "issued, can't observe" path the design doc calls out.
    #[test]
    fn present_exercise_report_without_failed_returns_all_ok() {
        use dormant_core::types::{BlankMode, DisplayId};

        let report = ExerciseReport {
            display: DisplayId("manual".into()),
            pre_phase: "active".into(),
            paused_rules: vec![],
            steps: vec![
                ExerciseStep {
                    command: "blank".into(),
                    blank_mode: Some(BlankMode::PowerOff),
                    returned_ok: true,
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Unconfirmable,
                    error: None,
                },
                ExerciseStep {
                    command: "wake".into(),
                    blank_mode: None,
                    returned_ok: true,
                    state_before: None,
                    state_after: None,
                    verdict: ExerciseVerdict::Unconfirmable,
                    error: None,
                },
            ],
        };
        let outcome = present_exercise_report(&report);
        assert_eq!(
            outcome,
            DoctorOutcome::AllOk,
            "no Failed step → AllOk (CLI exit 0) even with Unconfirmable rows"
        );
    }

    /// Parse the `--exercise <display>` subcommand from the CLI argv
    /// surface — confirms the parser wiring is correct (the handler is
    /// dispatched by `main.rs`, but the parse must still classify the
    /// subcommand into `DoctorSubcommand::Exercise`).
    #[test]
    fn parse_doctor_exercise_subcommand() {
        // The DoctorArgs struct itself can't be parsed in isolation
        // without the parent `dormantctl` argv surface; test the inner
        // enum's `FromArgMatches`-style classification via a direct
        // `clap::Parser::try_parse_from` invocation that omits the
        // outer wrapper.  This proves the subcommand is recognised.
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(subcommand)]
            sub: DoctorSubcommand,
        }
        let cli = Wrapper::try_parse_from(["dormantctl", "exercise", "mon"]).expect("parse");
        match cli.sub {
            DoctorSubcommand::Exercise { display } => assert_eq!(display, "mon"),
            other => panic!("expected Exercise, got {other:?}"),
        }
    }

    /// Task 11: the three new macOS read-only doctor arms must parse as
    /// their own subcommand variants under the EXACT kebab-case names the
    /// plan pins (`macos-idle`, `macos-display-sleep`, `macos-power`) — on
    /// every platform (parsing is unconditional; only the handler behind
    /// each variant is macOS-gated, mirroring the pre-existing `Ddcci`/
    /// `Kwin` pattern above).
    #[test]
    fn doctor_parses_all_macos_read_only_arms() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(subcommand)]
            sub: DoctorSubcommand,
        }

        let idle = Wrapper::try_parse_from(["dormantctl", "macos-idle"])
            .expect("macos-idle should parse")
            .sub;
        assert!(
            matches!(idle, DoctorSubcommand::MacosIdle),
            "expected MacosIdle, got {idle:?}"
        );

        let display_sleep = Wrapper::try_parse_from(["dormantctl", "macos-display-sleep"])
            .expect("macos-display-sleep should parse")
            .sub;
        assert!(
            matches!(display_sleep, DoctorSubcommand::MacosDisplaySleep),
            "expected MacosDisplaySleep, got {display_sleep:?}"
        );

        let power = Wrapper::try_parse_from(["dormantctl", "macos-power"])
            .expect("macos-power should parse")
            .sub;
        assert!(
            matches!(power, DoctorSubcommand::MacosPower),
            "expected MacosPower, got {power:?}"
        );
    }
}
