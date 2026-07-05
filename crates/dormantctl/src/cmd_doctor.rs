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
use dormant_core::config::schema::{Credentials, SensorConfig};
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

#[allow(clippy::too_many_lines)]
async fn run_async(args: &DoctorArgs) -> Result<DoctorOutcome> {
    match &args.subcommand {
        Some(DoctorSubcommand::Ddcci) => {
            #[cfg(target_os = "linux")]
            {
                let results = vec![dormant_doctor::probes::ddcci::probe_ddcci().await];
                print_table(&results);
                Ok(outcome(&results))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok(DoctorOutcome::NotSupported("ddcci".into()))
            }
        }
        Some(DoctorSubcommand::Usb { port, baud }) => {
            let results = vec![dormant_doctor::probes::usb::probe_usb(port, *baud).await];
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Mqtt) => {
            let (cfg, _creds) = load_config_and_creds(args)?;
            let results = dormant_doctor::probes::mqtt::probe_mqtt_all(&cfg).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Ha) => {
            let (cfg, creds) = load_config_and_creds(args)?;
            let results = dormant_doctor::probes::ha::probe_ha_all(&cfg, &creds).await;
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Config) => {
            let results = probe_config(args);
            print_table(&results);
            Ok(outcome(&results))
        }
        Some(DoctorSubcommand::Kwin) => Ok(DoctorOutcome::NotSupported("kwin-dpms".into())),
        Some(DoctorSubcommand::Samsung) => Ok(DoctorOutcome::NotSupported("samsung-tizen".into())),
        None => {
            // Bare doctor: run everything applicable.
            let (cfg, creds) = load_config_and_creds(args)?;
            let mut results = Vec::new();

            // Config probe first.
            let config_result = dormant_doctor::probes::config::probe_config_inner(&cfg, &creds);
            let config_ok = config_result.status != ProbeStatus::Fail;
            results.push(config_result);

            // Collect sensor probes.
            let mut sensor_futs: Vec<
                std::pin::Pin<Box<dyn futures_util::Future<Output = ProbeResult>>>,
            > = Vec::new();
            for (id, sensor_cfg) in &cfg.sensors {
                if !config_ok {
                    // Skip dependent probes when config is invalid.
                    let name = match sensor_cfg {
                        SensorConfig::Mqtt(_) => format!("mqtt {id}"),
                        SensorConfig::Ha(_) => format!("ha {id}"),
                        SensorConfig::UsbLd2410(usb_cfg) => format!("usb {}", usb_cfg.port),
                    };
                    results.push(ProbeResult::skip(name, "config invalid — fix config first"));
                    continue;
                }
                match sensor_cfg {
                    SensorConfig::Mqtt(mqtt_cfg) => {
                        let id = id.clone();
                        let cfg = mqtt_cfg.clone();
                        sensor_futs.push(Box::pin(async move {
                            dormant_doctor::probes::mqtt::probe_mqtt_one(&id, &cfg).await
                        }));
                    }
                    SensorConfig::Ha(ha_cfg) => {
                        let id = id.clone();
                        let cfg = ha_cfg.clone();
                        let creds = creds.clone();
                        sensor_futs.push(Box::pin(async move {
                            dormant_doctor::probes::ha::probe_ha_one(&id, &cfg, &creds).await
                        }));
                    }
                    SensorConfig::UsbLd2410(usb_cfg) => {
                        let port = usb_cfg.port.clone();
                        let baud = usb_cfg.baud;
                        sensor_futs.push(Box::pin(async move {
                            dormant_doctor::probes::usb::probe_usb(&port, baud).await
                        }));
                    }
                }
            }

            // Run sensor probes in parallel.
            if !sensor_futs.is_empty() {
                let sensor_results = futures_util::future::join_all(sensor_futs).await;
                results.extend(sensor_results);
            }

            // DDC/CI probe if any display uses ddcci (serial after sensors).
            #[cfg(target_os = "linux")]
            if config_ok {
                let has_ddcci = cfg
                    .displays
                    .values()
                    .any(|d| d.controllers.iter().any(|c| c == "ddcci"));
                if has_ddcci {
                    results.push(dormant_doctor::probes::ddcci::probe_ddcci().await);
                }
            }
            #[cfg(not(target_os = "linux"))]
            if cfg
                .displays
                .values()
                .any(|d| d.controllers.iter().any(|c| c == "ddcci"))
            {
                results.push(ProbeResult::skip(
                    "ddcci",
                    "DDC/CI is only supported on Linux in this release",
                ));
            }

            print_table(&results);
            Ok(outcome(&results))
        }
    }
}

// ── Config loading ──────────────────────────────────────────────────────────────

/// Load config and credentials using the same default-path logic as `validate`.
fn load_config_and_creds(args: &DoctorArgs) -> Result<(dormant_core::config::Config, Credentials)> {
    let config_path =
        paths::resolve_config_path(args.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    let (cfg, _warnings) = load_config(&config_path, Strictness::Warn)?;
    let creds = load_credentials(&creds_path)?;
    Ok((cfg, creds))
}

// ── Probe: config (CLI wrapper) ─────────────────────────────────────────────────

/// CLI-level config probe: loads config + credentials, then delegates to the
/// doctor crate.
fn probe_config(args: &DoctorArgs) -> Vec<ProbeResult> {
    let (cfg, creds) = match load_config_and_creds(args) {
        Ok(pair) => pair,
        Err(e) => return vec![ProbeResult::fail("config", format!("{e:#}"))],
    };
    vec![dormant_doctor::probes::config::probe_config_inner(
        &cfg, &creds,
    )]
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
    }

    // ── DoctorOutcome ───────────────────────────────────────────────────────

    #[test]
    fn outcome_all_pass_returns_all_ok() {
        let results = [ProbeResult::pass("a", ""), ProbeResult::skip("b", "")];
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
}
