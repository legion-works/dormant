//! dormantd — the dormant daemon binary.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use dormant_core::config::{Strictness, load_config};
use dormant_core::paths;
use dormantd::app::{self, App};
use dormantd::logging;

/// dormant daemon — proximity-driven display blanking.
#[derive(Parser, Debug)]
#[command(name = "dormantd", version, about)]
struct Cli {
    /// Path to the config file. Defaults to
    /// `$XDG_CONFIG_HOME/dormant/config.toml`, then `/etc/dormant/config.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to the credentials file. Defaults to `credentials.toml` beside the
    /// config file.
    #[arg(long)]
    credentials: Option<PathBuf>,

    /// Validate the config and exit (non-zero on any error).
    #[arg(long)]
    validate_only: bool,

    /// Treat unknown config keys as warnings instead of errors.
    #[arg(long)]
    lenient_keys: bool,

    /// Emit structured JSON logs.
    #[arg(long)]
    log_json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let strictness = if cli.lenient_keys {
        Strictness::Warn
    } else {
        Strictness::Strict
    };

    let config_path = match paths::resolve_config_path(cli.config.as_deref()) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let creds_path = cli
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    let level = peek_log_level(&config_path, strictness);
    if let Err(e) = logging::init(&level, cli.log_json) {
        eprintln!("failed to initialise logging: {e}");
        return ExitCode::FAILURE;
    }

    if cli.validate_only {
        let report = app::validate_only(&config_path, &creds_path, strictness);
        let mut out = String::new();
        report.render(&mut out);
        print!("{out}");
        return ExitCode::from(u8::try_from(report.exit_code()).unwrap_or(1));
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(event = "runtime_init_failed", error = %e);
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        let app = match App::build(config_path, creds_path, strictness) {
            Ok(app) => app,
            Err(e) => {
                tracing::error!(event = "startup_failed", error = %e);
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        // Single-instance guard: acquire BEFORE the daemon starts touching
        // physical displays. Held for the entire process lifetime (bound in
        // this async block scope; released on process exit — the kernel also
        // releases the flock on process death, so crash-safe).
        let _lock = match dormantd::single_instance::acquire(&paths::default_lock_path()) {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(event = "single_instance_lock_failed", error = %e);
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        };
        tracing::info!(event = "daemon_starting", config = %app.config_path().display());
        match app.run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(event = "daemon_failed", error = %e);
                ExitCode::FAILURE
            }
        }
    })
}

/// Best-effort read of `daemon.log_level` before logging is initialised.
fn peek_log_level(config_path: &std::path::Path, strictness: Strictness) -> String {
    load_config(config_path, strictness)
        .map_or_else(|_| "info".to_string(), |(cfg, _)| cfg.daemon.log_level)
}
