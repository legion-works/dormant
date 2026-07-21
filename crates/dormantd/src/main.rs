//! dormantd — the dormant daemon binary.
//!
//! Wires configuration → sensors → zones → rules → displays, with post-probe
//! display validation, hot config reload, and a user-activity inhibitor.
//!
//! Boot sequence (spec §5.1): stale-gamma restore → `peek_boot_options`
//! (cheap, pre-runtime) → `boot_guard::prepare` (records this start, decides
//! the crash-loop verdict, chooses which config to attempt) → peek the CHOSEN
//! path's log level → `logging::init` → emit `prepare`'s deferred log events →
//! `runtime.block_on(boot::boot(plan, inputs))`, which owns the actual
//! build/lock/start/rollback machinery (`dormantd::boot`).

#[allow(dead_code)] // T8 anchors this module in App startup.
mod coordination_poll;
mod main_sequence;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser;
use dormant_core::config::{Strictness, load_config};
use dormant_core::paths;
use dormantd::app;
use dormantd::boot::{self, BootOutcome};
use dormantd::boot_guard::{self, BootInputs, BootPlan, DeferredEvent};
use dormantd::gamma_recovery;
use dormantd::logging;
use dormantd::sd_notify::SdNotify;
use main_sequence::{StartupInputs, run_boot_with_shutdown_restore, run_startup_sequence};

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

    let state_dir = paths::state_dir();
    let lock_path = paths::default_lock_path();

    run_startup_sequence(
        StartupInputs {
            config_path,
            creds_path,
            state_dir,
            strictness,
        },
        |state_dir| {
            gamma_recovery::restore_stale_breadcrumb(
                state_dir,
                &gamma_recovery::RealGammaSystemRestore,
                "startup",
            )
        },
        peek_boot_options,
        |inputs, boot_options| {
            boot_guard::prepare(
                &inputs.config_path,
                &inputs.creds_path,
                &inputs.state_dir,
                inputs.strictness,
                boot_options.lkg_rollback_enabled,
            )
        },
        |inputs, boot_options, plan, startup_gamma_event| {
            // The common and immediate-rollback paths already peeked the chosen
            // config. Only a prepare-time rollback changes the chosen path.
            let level = if plan.chosen_config == inputs.config_path {
                boot_options.log_level
            } else {
                peek_log_level(&plan.chosen_config, inputs.strictness)
            };
            if let Err(e) = logging::init(&level, cli.log_json) {
                eprintln!("failed to initialise logging: {e}");
                return ExitCode::FAILURE;
            }

            emit_deferred_events(&plan.deferred_events);
            if let Some(event) = startup_gamma_event.as_ref() {
                gamma_recovery::emit_deferred_event(event);
            }

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

            let boot_inputs = BootInputs {
                creds_path: inputs.creds_path,
                strictness: inputs.strictness,
                state_dir: inputs.state_dir,
                lock_path,
                sd_notify: SdNotify::from_env(),
                observations: dormant_core::observation::ObservationHub::new(64),
            };

            runtime.block_on(run_to_completion(plan, boot_inputs))
        },
    )
}

/// `block_on(boot(plan, inputs))` plus the outcome dispatch (spec §5.1).
///
/// The ordering seam wraps the whole verdict dispatch so lock/build failures,
/// boot errors, clean shutdown, and a failed run-loop join all converge on the
/// same shutdown restore.
async fn run_to_completion(plan: BootPlan, inputs: BootInputs) -> ExitCode {
    // Captured before `plan`/`inputs` move into `boot::boot` below
    // (rollback-recovery plan, Task 1 §6 for `operator_config`; Task 8 for
    // `state_dir`): `operator_config` is the REAL operator path, for the
    // `reload_config` log field — distinct from `used_config`, which keeps
    // reporting whichever source actually booted generation 0 (unchanged
    // meaning). `state_dir` is needed again below, after `inputs` is gone.
    let operator_config = plan.operator_config.clone();
    let state_dir = inputs.state_dir.clone();
    run_boot_with_shutdown_restore(
        // `boot()` owns the full assembly state across awaits; boxing keeps the
        // top-level shutdown wrapper future small without changing ownership.
        Box::pin(async move {
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
                        reload_config = %operator_config.display(),
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
        }),
        || {
            let event = gamma_recovery::restore_stale_breadcrumb(
                &state_dir,
                &gamma_recovery::RealGammaSystemRestore,
                "shutdown",
            );
            if let Some(event) = event.as_ref() {
                gamma_recovery::emit_deferred_event(event);
            }
        },
    )
    .await
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
