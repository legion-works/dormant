//! dormantctl — CLI companion for dormantd.
//!
//! Communicates with a running `dormantd` daemon over a Unix domain socket
//! using line-delimited JSON.  Supports status queries, pause/resume, force
//! blank/wake, config reload, event watching, and offline config validation.

#![warn(missing_docs)]

mod client;
mod cmd_blank;
mod cmd_pause;
mod cmd_status;
mod cmd_validate;
mod cmd_watch;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dormant_core::ipc_proto::IpcRequest;

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let socket_path = resolve_socket(cli.socket.as_deref());

    match cli.command {
        Command::Status { json } => {
            if let Err(e) = cmd_status::run(&socket_path, json) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Pause { duration, rule } => {
            let dur = duration.map(std::convert::Into::into);
            if let Err(e) = cmd_pause::run_pause(&socket_path, dur, rule) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Resume { rule } => {
            if let Err(e) = cmd_pause::run_resume(&socket_path, rule) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Blank { display } => {
            if let Err(e) = cmd_blank::run_blank(&socket_path, &display) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Wake { display } => {
            if let Err(e) = cmd_blank::run_wake(&socket_path, &display) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Reload => match client::send_request(&socket_path, &IpcRequest::Reload) {
            Ok(resp) if resp.ok => println!("ok"),
            Ok(resp) => eprintln!("error: {}", resp.error.as_deref().unwrap_or("unknown")),
            Err(e) => eprintln!("error: {e}"),
        },
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
            if let Err(e) = cmd_validate::run(&args) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Watch { json } => {
            if let Err(e) = cmd_watch::run(&socket_path, json) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

/// Resolve the socket path from an explicit arg or default chain.
fn resolve_socket(explicit: Option<&std::path::Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let mut p = PathBuf::from(runtime_dir);
        p.push("dormant.sock");
        return p;
    }
    PathBuf::from("/run/dormant/dormant.sock")
}
