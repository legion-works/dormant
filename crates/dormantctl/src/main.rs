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
mod cmd_switch;
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
}

/// Active-sampling consent commands.
#[derive(Subcommand, Debug)]
enum WearCommand {
    /// Request portal consent and enable active sampling.
    EnableSampling,
    /// Disable active sampling.
    DisableSampling {
        /// Delete the stored consent record after closing the session.
        #[arg(long)]
        forget: bool,
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
    /// Blank a display.
    ///
    /// Issue #124 split the blank policy: by default this command walks the
    /// configured render/stage/controller ladder from its first stage
    /// (the safe `Soft` mode) and never hard-powers the panel.  Pass
    /// `--hard` to issue the operator-override `PowerOff` (the `Hard`
    /// mode) — this requires a confirmation prompt in a TTY unless
    /// `--yes` is also given, and is the equivalent of the tray/web
    /// "Force blank" button.
    Blank {
        /// Display id to blank.
        display: String,
        /// Issue the operator-override `PowerOff` (`Hard` mode) instead of
        /// the safe ladder (`Soft` mode).  Triggers a confirmation prompt
        /// in a TTY; pass `--yes` to bypass for scripts/CI.
        #[arg(long)]
        hard: bool,
        /// Skip the `--hard` confirmation prompt.  Only valid with
        /// `--hard` (clap rejects `--yes` alone).
        #[arg(long, requires = "hard")]
        yes: bool,
    },
    /// Force-wake a display.
    Wake {
        /// Display id to wake.
        display: String,
    },
    /// Write the local input code to pull a shared display to this
    /// machine.  Use `--to-peer` to push the display away by writing
    /// the peer input code (requires `shared_peer_input_write_code`).
    Switch {
        /// Shared display id.
        display: String,
        /// Write the peer input code instead of the local one.
        #[arg(long)]
        to_peer: bool,
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
    /// Manage active-time wear sampling consent.
    Wear {
        #[command(subcommand)]
        subcommand: WearCommand,
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
        Command::Blank { display, hard, yes } => {
            cmd_blank::run_blank(&socket_path, &display, hard, yes)
        }
        Command::Wake { display } => cmd_blank::run_wake(&socket_path, &display),
        Command::Switch { display, to_peer } => {
            if to_peer {
                cmd_switch::run_peer(&socket_path, &display)
            } else {
                cmd_switch::run(&socket_path, &display)
            }
        }
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
        Command::Wear { subcommand } => match subcommand {
            WearCommand::EnableSampling => dormantctl::cmd_wear::run_enable(&socket_path),
            WearCommand::DisableSampling { forget } => {
                dormantctl::cmd_wear::run_disable(&socket_path, forget)
            }
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

    // ── `launchd install` / `launchd uninstall` parsing ──────────
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

    #[test]
    fn parse_blank_soft_default() {
        // Bare `dormantctl blank <display>` — the safe-soft default
        // (issue #124).  No prompt, no flags.
        let cli = Cli::try_parse_from(["dormantctl", "blank", "monitor"]).unwrap();
        match cli.command {
            Command::Blank { display, hard, yes } => {
                assert_eq!(display, "monitor");
                assert!(!hard, "default mode must be soft (hard=false)");
                assert!(!yes, "yes must be false by default");
            }
            _ => panic!("expected Blank command"),
        }
    }

    #[test]
    fn parse_blank_hard_flag() {
        let cli = Cli::try_parse_from(["dormantctl", "blank", "monitor", "--hard"]).unwrap();
        match cli.command {
            Command::Blank { display, hard, yes } => {
                assert_eq!(display, "monitor");
                assert!(hard, "--hard must set hard=true");
                assert!(!yes, "yes must be false unless --yes is given");
            }
            _ => panic!("expected Blank command"),
        }
    }

    #[test]
    fn parse_blank_hard_with_yes() {
        let cli =
            Cli::try_parse_from(["dormantctl", "blank", "monitor", "--hard", "--yes"]).unwrap();
        match cli.command {
            Command::Blank { display, hard, yes } => {
                assert_eq!(display, "monitor");
                assert!(hard);
                assert!(yes);
            }
            _ => panic!("expected Blank command"),
        }
    }

    #[test]
    fn parse_blank_yes_without_hard_rejected() {
        // `--yes` without `--hard` is a clap conflict (requires="hard"
        // on the yes field).  Pin: a regression that drops the requires
        // attribute would silently let `--yes` through and change the
        // default for `--yes`-only callers.
        let err = Cli::try_parse_from(["dormantctl", "blank", "monitor", "--yes"])
            .expect_err("--yes alone must be rejected by clap");
        let msg = format!("{err}");
        assert!(
            msg.contains("--yes") || msg.contains("required") || msg.contains("--hard"),
            "error should reference --yes/--hard, got: {msg}"
        );
    }

    #[test]
    fn parse_switch_to_peer() {
        let cli = Cli::try_parse_from(["dormantctl", "switch", "monitor", "--to-peer"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Switch { display, to_peer: true } if display == "monitor"
        ));
    }

    #[test]
    fn parse_switch_plain() {
        let cli = Cli::try_parse_from(["dormantctl", "switch", "monitor"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Switch { display, to_peer: false } if display == "monitor"
        ));
    }
}
