//! dormantd — the dormant daemon binary.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor.
//!
//! Boot sequence (spec §5.1): `peek_boot_options` (cheap, pre-runtime) →
//! `boot_guard::prepare` (records this start, decides the crash-loop
//! verdict, chooses which config to attempt) → peek the CHOSEN path's log
//! level → `logging::init` → emit `prepare`'s deferred log events →
//! `runtime.block_on(boot::boot(plan, inputs))`, which owns the actual
//! build/lock/start/rollback machinery (`dormantd::boot`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser;
use dormant_core::config::{Strictness, load_config};
use dormant_core::paths;
use dormantd::app;
use dormantd::boot::{self, BootOutcome};
use dormantd::boot_guard::{self, BootInputs, BootPlan, DeferredEvent};
use dormantd::logging;
use dormantd::sd_notify::SdNotify;

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

/// Number of tokio worker threads.
///
/// dormant is I/O-bound (MQTT, HA WebSocket, Unix-socket IPC, the axum
/// web server, the reload watcher, and forwarder tasks) and not
/// CPU-bound, so two async workers is ample.  Capping the worker count
/// also caps the number of glibc malloc arenas — each worker thread is
/// a fresh arena — which complements the systemd
/// `MALLOC_ARENA_MAX=2` setting by shrinking in-process baseline RSS.
/// Deliberately a constant, not a config key: the runtime is built
/// before the config file is parsed (`App::build` runs inside
/// `block_on`), so a config-driven value would need a pre-parse and
/// isn't worth that complexity here.
const WORKER_THREADS: usize = 2;

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

    // `--validate-only` returns before boot_guard/crash-loop.json is even
    // touched (spec §5.1 point 0, F12, explicit) — untouched by this task.
    if cli.validate_only {
        let level = peek_log_level(&config_path, strictness);
        if let Err(e) = logging::init(&level, cli.log_json) {
            eprintln!("failed to initialise logging: {e}");
            return ExitCode::FAILURE;
        }
        let report = app::validate_only(&config_path, &creds_path, strictness);
        let mut out = String::new();
        report.render(&mut out);
        print!("{out}");
        return ExitCode::from(u8::try_from(report.exit_code()).unwrap_or(1));
    }

    // Cheap pre-`prepare` peek (spec §5.1, confirm-round row): the
    // crash-loop rollback gate must be known BEFORE `prepare` runs, but
    // logging doesn't exist yet either. Defaults the gate true if the
    // config cannot load at all — an unloadable config is exactly the case
    // this machinery must still work for.
    let boot_options = peek_boot_options(&config_path, strictness);

    let state_dir = paths::state_dir();
    let lock_path = paths::default_lock_path();

    let plan = boot_guard::prepare(
        &config_path,
        &creds_path,
        &state_dir,
        strictness,
        boot_options.lkg_rollback_enabled,
    );

    // Peek the log level of the CHOSEN path (spec §5.1: "against the CHOSEN
    // path"). When `prepare` chose the original config (the common case,
    // and also the immediate-rollback case — that failure is only
    // discovered later, inside `boot()`, so the chosen path IS still the
    // original config here), `boot_options.log_level` already IS that
    // peek — reuse it rather than reading the file twice. Only a
    // `RollBack`/`ContinueRollback` verdict (chosen = the LKG file) needs a
    // fresh peek against a DIFFERENT path.
    let level = if plan.chosen_config == config_path {
        boot_options.log_level.clone()
    } else {
        peek_log_level(&plan.chosen_config, strictness)
    };
    if let Err(e) = logging::init(&level, cli.log_json) {
        eprintln!("failed to initialise logging: {e}");
        return ExitCode::FAILURE;
    }

    emit_deferred_events(&plan.deferred_events);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(event = "runtime_init_failed", error = %e);
            return ExitCode::FAILURE;
        }
    };

    let inputs = BootInputs {
        creds_path,
        strictness,
        state_dir,
        lock_path,
        sd_notify: SdNotify::from_env(),
    };

    runtime.block_on(run_to_completion(plan, inputs))
}

/// `block_on(boot(plan, inputs))` plus the outcome dispatch (spec §5.1) —
/// split out of `main` purely to keep `main` under the line-count gate; no
/// behavioral seam.
async fn run_to_completion(plan: BootPlan, inputs: BootInputs) -> ExitCode {
    match boot::boot(plan, inputs).await {
        Ok(BootOutcome::LockFailed) => ExitCode::from(1),
        Ok(BootOutcome::BuildFailed(msg)) => {
            tracing::error!(event = "startup_failed", error = %msg);
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
        Ok(BootOutcome::Started {
            handle,
            join,
            used_config,
            rolled_back,
        }) => {
            tracing::info!(
                event = "daemon_starting",
                config = %used_config.display(),
                rolled_back,
            );
            let result = join.await.context("run loop panicked");
            drop(handle);
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(event = "daemon_failed", error = %e);
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            tracing::error!(event = "daemon_failed", error = %e);
            ExitCode::FAILURE
        }
    }
}

/// Emit `prepare`'s deferred log events (spec §5.1: recorded/decided before
/// logging existed, emitted here once it does). The IMMEDIATE-rollback
/// `config_rollback_boot` event is a SEPARATE emission, direct from
/// `boot()` itself (spec §5.1 point 3 — that failure is discovered live,
/// after this function has already run); both share the same grep-stable
/// literal by design.
fn emit_deferred_events(events: &[DeferredEvent]) {
    for event in events {
        match event {
            DeferredEvent::CrashLoopDetected { count } => {
                tracing::warn!(event = "crash_loop_detected", count = *count);
            }
            DeferredEvent::RollbackBoot {
                failed_fp,
                lkg_fp,
                detail,
            } => {
                tracing::error!(
                    event = "config_rollback_boot",
                    failed_fp = ?failed_fp,
                    lkg_fp = ?lkg_fp,
                    detail = %detail,
                );
            }
            DeferredEvent::RollbackContinued => {
                tracing::info!(event = "config_rollback_continued");
            }
            DeferredEvent::RollbackRetry { message } => {
                tracing::warn!(event = "config_rollback_retry", message = %message);
            }
            DeferredEvent::LkgMissingRollbackDisarmed => {
                tracing::warn!(event = "lkg_missing_rollback_disarmed");
            }
        }
    }
}

/// Best-effort read of `daemon.log_level` before logging is initialised.
fn peek_log_level(config_path: &std::path::Path, strictness: Strictness) -> String {
    load_config(config_path, strictness)
        .map_or_else(|_| "info".to_string(), |(cfg, _)| cfg.daemon.log_level)
}

/// The cheap pre-`prepare` load's two outputs (spec §5.1 confirm-round
/// row): the log level (reused by `main` when `prepare` doesn't diverge
/// from the original config) and `watchdog.lkg_rollback_enabled` (the
/// counted-rollback gate `prepare` needs). An unloadable config defaults
/// the gate `true` — the crash-loop machinery must work precisely when the
/// config itself is unloadable.
struct BootOptions {
    log_level: String,
    lkg_rollback_enabled: bool,
}

fn peek_boot_options(config_path: &std::path::Path, strictness: Strictness) -> BootOptions {
    load_config(config_path, strictness).map_or_else(
        |_| BootOptions {
            log_level: "info".to_string(),
            lkg_rollback_enabled: true,
        },
        |(cfg, _)| BootOptions {
            log_level: cfg.daemon.log_level,
            lkg_rollback_enabled: cfg.watchdog.lkg_rollback_enabled,
        },
    )
}
