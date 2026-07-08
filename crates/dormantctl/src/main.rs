//! dormantctl — CLI companion for dormantd.
//!
//! Communicates with a running `dormantd` daemon over a Unix domain socket
//! using line-delimited JSON.  Supports status queries, pause/resume, force
//! blank/wake, config reload, event watching, and offline config validation.

#![warn(missing_docs)]

mod cmd_blank;
mod cmd_doctor;
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

        #[command(subcommand)]
        subcommand: Option<cmd_doctor::DoctorSubcommand>,
    },
}

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
        },
        Command::Doctor {
            config,
            credentials,
            subcommand,
        } => {
            let args = cmd_doctor::DoctorArgs {
                config,
                credentials,
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
        let cli = Cli::try_parse_from(["dormantctl", "pair", "samsung", "10.1.1.7"]).unwrap();
        match cli.command {
            Command::Pair {
                target: PairTarget::Samsung { host },
                ..
            } => assert_eq!(host, "10.1.1.7"),
            _ => panic!("expected Pair command"),
        }
    }
}
