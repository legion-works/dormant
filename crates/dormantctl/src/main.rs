//! dormantctl — CLI companion for dormantd.
//!
//! Communicates with a running `dormantd` daemon over a Unix domain socket
//! using line-delimited JSON.  Supports status queries, pause/resume, force
//! blank/wake, config reload, event watching, and offline config validation.

#![warn(missing_docs)]

mod cmd_blank;
mod cmd_doctor;
mod cmd_emergency_wake;
mod cmd_launchd;
mod cmd_pair;
mod cmd_pause;
mod cmd_status;
mod cmd_validate;
mod cmd_watch;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::paths;

/// dormantctl — control the dormant daemon.
#[derive(Parser, Debug)]
#[command(name = "dormantctl", version, about)]
struct Cli {
    /// Path to the daemon's Unix socket.
    ///
    /// Defaults to `$XDG_RUNTIME_DIR/dormant.sock`, then
    /// `/run/dormant/dormant.sock`.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// Pairing target device.
#[derive(clap::Subcommand, Debug)]
enum PairTarget {
    /// Pair a Samsung Tizen TV.
    Samsung {
        /// TV hostname or IP address.
        host: String,
    },
    /// Pair with another dormant instance discovered on the local network.
    Instance {
        /// Discovered peer display name, or the local display name with --open.
        name: String,
        /// Pairing code read from the responding instance.
        #[arg(long, required_unless_present = "open")]
        code: Option<String>,
        /// Select a specific discovered instance when names are duplicated.
        #[arg(long)]
        instance_id: Option<String>,
        /// Open a local responder pairing window and print its one-time code.
        #[arg(long, conflicts_with_all = ["code", "instance_id"])]
        open: bool,
    },
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show daemon status (sensors, zones, displays).
    Status {
        /// Output raw JSON instead of a human table.
        #[arg(long)]
        json: bool,
    },
    /// Pause blanking (optional rule, optional duration).
    Pause {
        /// Duration like "2h", "90m", "30s" (humantime format).
        duration: Option<humantime::Duration>,

        /// Only pause this rule.
        #[arg(long)]
        rule: Option<String>,
    },
    /// Resume blanking (optional rule).
    Resume {
        /// Only resume this rule.
        #[arg(long)]
        rule: Option<String>,
    },
    /// Force-blank a display.
    Blank {
        /// Display id to blank.
        display: String,
    },
    /// Force-wake a display.
    Wake {
        /// Display id to wake.
        display: String,
    },
    /// Trigger a config reload.
    Reload,
    /// Validate configuration offline (no daemon needed).
    Validate {
        /// Path to the config file.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Path to the credentials file.
        #[arg(long)]
        credentials: Option<PathBuf>,

        /// Treat unknown config keys as warnings instead of errors.
        #[arg(long)]
        lenient_keys: bool,
    },
    /// Watch the daemon event stream.
    Watch {
        /// Output raw JSON events instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },
    /// Pair with a device that needs an auth token (e.g. a Samsung TV).
    Pair {
        #[command(subcommand)]
        target: PairTarget,

        /// Path to the config file.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Path to the credentials file.
        #[arg(long)]
        credentials: Option<PathBuf>,
    },
    /// Diagnose hardware and connectivity.
    Doctor {
        /// Path to the config file.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Path to the credentials file.
        #[arg(long)]
        credentials: Option<PathBuf>,

        /// Write a ready-to-file bug report draft after the offline probe
        /// set. See `cmd_doctor::DoctorArgs::report_issue`.
        #[arg(
            long,
            value_name = "PATH",
            num_args = 0..=1,
            default_missing_value = "",
            conflicts_with = "draft_feature"
        )]
        report_issue: Option<String>,

        /// Write a ready-to-file feature request draft after the offline
        /// probe set. See `cmd_doctor::DoctorArgs::draft_feature`.
        #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
        draft_feature: Option<String>,

        #[command(subcommand)]
        subcommand: Option<cmd_doctor::DoctorSubcommand>,
    },
    /// Force-wake every display — one-command panic recovery.  Bypasses
    /// sensor logic and the rules engine to send a wake command to every
    /// configured display.  Bind to a global shortcut (KDE `KGlobalAccel`
    /// / XDG `GlobalShortcuts` portal) for an emergency "screens on now"
    /// key.
    ///
    /// Routes through the IPC fast path first; if the daemon is wedged or
    /// unreachable, falls back to constructing display controllers
    /// directly from the loaded config and credentials.
    EmergencyWake {
        /// Path to the config file (used for the direct-hardware fallback).
        #[arg(long)]
        config: Option<PathBuf>,

        /// Path to the credentials file (used for the direct-hardware fallback).
        #[arg(long)]
        credentials: Option<PathBuf>,

        /// Treat unknown config keys as warnings instead of errors.
        #[arg(long)]
        lenient_keys: bool,
    },
    /// Install or remove the macOS launchd `LaunchAgent`. macOS only — parses
    /// on every platform (so `--help` is always accurate) but the handler
    /// reports "not yet supported" (exit 3) off macOS, matching the
    /// `doctor macos-*` arms.
    Launchd {
        #[command(subcommand)]
        subcommand: cmd_launchd::LaunchdSubcommand,
    },
}

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    let cli = Cli::parse();
    let socket_path = paths::resolve_socket_path(cli.socket.as_deref());

    let result = match cli.command {
        Command::Status { json } => cmd_status::run(&socket_path, json),
        Command::Pause { duration, rule } => {
            let dur = duration.map(std::convert::Into::into);
            cmd_pause::run_pause(&socket_path, dur, rule)
        }
        Command::Resume { rule } => cmd_pause::run_resume(&socket_path, rule),
        Command::Blank { display } => cmd_blank::run_blank(&socket_path, &display),
        Command::Wake { display } => cmd_blank::run_wake(&socket_path, &display),
        Command::Reload => {
            match dormantctl::client::send_request(&socket_path, &IpcRequest::Reload) {
                Ok(resp) if resp.ok => {
                    println!("ok");
                    Ok(())
                }
                Ok(resp) => Err(anyhow::anyhow!(
                    "{}",
                    resp.error.as_deref().unwrap_or("unknown")
                )),
                Err(e) => Err(e),
            }
        }
        Command::Validate {
            config,
            credentials,
            lenient_keys,
        } => {
            let args = cmd_validate::ValidateArgs {
                config,
                credentials,
                lenient_keys,
            };
            cmd_validate::run(&args)
        }
        Command::Watch { json } => cmd_watch::run(&socket_path, json),
        Command::Pair {
            target,
            config,
            credentials,
        } => match target {
            PairTarget::Samsung { host } => cmd_pair::run(&cmd_pair::PairArgs {
                config,
                credentials,
                host,
            }),
            PairTarget::Instance {
                name,
                code,
                instance_id,
                open,
            } => cmd_pair::run_instance(
                &socket_path,
                &name,
                code.as_deref(),
                instance_id.as_deref(),
                open,
            ),
        },
        Command::Doctor {
            config,
            credentials,
            report_issue,
            draft_feature,
            subcommand,
        } => {
            // The Exercise subcommand needs the resolved socket path (the
            // global `--socket` flag), so we dispatch it directly from
            // here instead of round-tripping through `cmd_doctor::run`.
            // Every other subcommand flows through the regular `run` path.
            if let Some(cmd_doctor::DoctorSubcommand::Exercise { display }) = subcommand.as_ref() {
                let display = display.clone();
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                let result = rt.block_on(async {
                    cmd_doctor::run_exercise_with_socket(&socket_path, &display)
                });
                return match result {
                    Ok(cmd_doctor::DoctorOutcome::AllOk) => ExitCode::SUCCESS,
                    Ok(cmd_doctor::DoctorOutcome::SomeFailed) => {
                        eprintln!("some probes failed");
                        ExitCode::FAILURE
                    }
                    Ok(cmd_doctor::DoctorOutcome::NotSupported(controller)) => {
                        eprintln!(
                            "not yet supported: requires the {controller} controller \
                             (pending hardware verification milestone)"
                        );
                        ExitCode::from(3)
                    }
                    Err(e) => {
                        eprintln!("error: {e:#}");
                        ExitCode::FAILURE
                    }
                };
            }

            let args = cmd_doctor::DoctorArgs {
                config,
                credentials,
                report_issue,
                draft_feature,
                subcommand,
            };
            match cmd_doctor::run(&args) {
                Ok(outcome) => match outcome {
                    cmd_doctor::DoctorOutcome::AllOk => Ok(()),
                    cmd_doctor::DoctorOutcome::SomeFailed => {
                        // Error is already printed as the table; signal exit 1.
                        Err(anyhow::anyhow!("some probes failed"))
                    }
                    cmd_doctor::DoctorOutcome::NotSupported(controller) => {
                        eprintln!(
                            "not yet supported: requires the {controller} controller \
                             (pending hardware verification milestone)"
                        );
                        return ExitCode::from(3);
                    }
                },
                Err(e) => Err(e),
            }
        }
        Command::EmergencyWake {
            config,
            credentials,
            lenient_keys,
        } => {
            let args = cmd_emergency_wake::EmergencyWakeArgs {
                socket: cli.socket.clone(),
                config,
                credentials,
                lenient_keys,
            };
            match cmd_emergency_wake::run(&args) {
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            }
        }
        Command::Launchd { subcommand } => match cmd_launchd::run(&subcommand) {
            Ok(cmd_launchd::LaunchdOutcome::Installed(paths)) => {
                println!("installed {}", paths.daemon.display());
                println!("installed {}", paths.tray.display());
                Ok(())
            }
            Ok(cmd_launchd::LaunchdOutcome::Uninstalled(result)) => {
                if result.daemon_removed {
                    println!("removed {}", result.paths.daemon.display());
                } else {
                    println!("not installed: {}", result.paths.daemon.display());
                }
                if result.tray_removed {
                    println!("removed {}", result.paths.tray.display());
                } else {
                    println!("not installed: {}", result.paths.tray.display());
                }
                Ok(())
            }
            Ok(cmd_launchd::LaunchdOutcome::NotSupported) => {
                eprintln!("not yet supported: launchd is macOS-only");
                return ExitCode::from(3);
            }
            Err(e) => Err(e),
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("error: {msg}");
            // Connection-refused / daemon-not-running → exit 2.
            if msg.contains("daemon not running") || msg.contains("Connection refused") {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_pair_samsung() {
        let cli = Cli::try_parse_from(["dormantctl", "pair", "samsung", "192.0.2.7"]).unwrap();
        match cli.command {
            Command::Pair {
                target: PairTarget::Samsung { host },
                ..
            } => assert_eq!(host, "192.0.2.7"),
            _ => panic!("expected Pair command"),
        }
    }

    #[test]
    fn parse_pair_instance_peer() {
        let cli = Cli::try_parse_from([
            "dormantctl",
            "pair",
            "instance",
            "Office-Mac",
            "--code",
            "ABCD1234",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Pair {
                target: PairTarget::Instance { name, code: Some(code), open: false, .. },
                ..
            } if name == "Office-Mac" && code == "ABCD1234"
        ));
    }

    #[test]
    fn parse_pair_instance_open() {
        let cli = Cli::try_parse_from(["dormantctl", "pair", "instance", "Office-Mac", "--open"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Command::Pair {
                target: PairTarget::Instance { name, code: None, open: true, .. },
                ..
            } if name == "Office-Mac"
        ));
    }

    // ── Task 12: `launchd install` / `launchd uninstall` parsing ──────────
    //
    // Parsing is unconditional on every platform (mirrors the
    // `doctor macos-*` arms in cmd_doctor.rs) — only the handler behind it
    // is macOS-gated (cmd_launchd::run), so `--help` stays accurate
    // everywhere and these tests run on Linux CI too.

    #[test]
    fn parse_launchd_install() {
        let cli = Cli::try_parse_from(["dormantctl", "launchd", "install"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Launchd {
                subcommand: cmd_launchd::LaunchdSubcommand::Install
            }
        ));
    }

    #[test]
    fn parse_launchd_uninstall() {
        let cli = Cli::try_parse_from(["dormantctl", "launchd", "uninstall"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Launchd {
                subcommand: cmd_launchd::LaunchdSubcommand::Uninstall
            }
        ));
    }
}
