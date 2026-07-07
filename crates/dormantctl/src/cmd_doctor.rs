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
use dormant_core::paths;
use dormant_doctor::{ProbeResult, ProbeStatus};

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
    /// Probe Samsung Tizen display (not yet supported).
    Samsung,
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
            #[cfg(target_os = "linux")]
            {
                let results = vec![dormant_doctor::probe_ddcci().await];
                print_table(&results);
                Ok(outcome(&results))
            }
            #[cfg(not(target_os = "linux"))]
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
        Some(DoctorSubcommand::Samsung) => Ok(DoctorOutcome::NotSupported("samsung-tizen".into())),
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

/// Print a table of probe results.
fn print_table(results: &[ProbeResult]) {
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

    println!("{table}");
}

/// Determine the overall outcome from probe results.
fn outcome(results: &[ProbeResult]) -> DoctorOutcome {
    if results.iter().any(|r| r.status == ProbeStatus::Fail) {
        DoctorOutcome::SomeFailed
    } else {
        DoctorOutcome::AllOk
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Table formatting ────────────────────────────────────────────────────

    #[test]
    fn table_contains_glyphs() {
        let results = vec![
            ProbeResult::pass("test-pass", "all good"),
            ProbeResult::fail("test-fail", "something broke"),
            ProbeResult::skip("test-skip", "not applicable"),
            ProbeResult::not_supported("test-na", "not on this platform"),
        ];

        // Print to string and check glyphs.
        let mut table = Table::new();
        table
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["Probe", "Status", "Detail"]);
        for r in &results {
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

        let output = table.to_string();
        assert!(output.contains('✓'), "table should contain checkmark");
        assert!(output.contains('✗'), "table should contain X mark");
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
            output.contains("N/A"),
            "table should contain NotSupported glyph"
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
}
