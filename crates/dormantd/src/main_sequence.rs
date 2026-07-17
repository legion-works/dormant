//! Testable ordering shell for the daemon binary's startup and shutdown.
//!
//! The real effects stay in `main.rs`; these helpers only make the ordering
//! between them structural so platform-neutral tests can pin it.

use std::future::Future;
use std::path::{Path, PathBuf};

use dormant_core::config::Strictness;

pub(super) struct StartupInputs {
    pub(super) config_path: PathBuf,
    pub(super) creds_path: PathBuf,
    pub(super) state_dir: PathBuf,
    pub(super) strictness: Strictness,
}

/// Enforce startup's non-negotiable order before entering any terminal path.
///
/// `boot` owns the remaining logging/runtime/boot stage. Its early returns
/// therefore cannot bypass the stale-gamma restore, config peek, or
/// `boot_guard::prepare` calls that precede it here.
pub(super) fn run_startup_sequence<Config, Prepared, Deferred, Output>(
    inputs: StartupInputs,
    restore_stale_gamma: impl FnOnce(&Path) -> Deferred,
    load_config: impl FnOnce(&Path, Strictness) -> Config,
    prepare: impl FnOnce(&StartupInputs, &Config) -> Prepared,
    boot: impl FnOnce(StartupInputs, Config, Prepared, Deferred) -> Output,
) -> Output {
    let deferred = restore_stale_gamma(&inputs.state_dir);
    let config = load_config(&inputs.config_path, inputs.strictness);
    let prepared = prepare(&inputs, &config);
    boot(inputs, config, prepared, deferred)
}

/// Await the complete boot verdict dispatch before running shutdown restore.
///
/// Logging/runtime initialization failures occur before this wrapper because
/// no controller can have written a new breadcrumb yet. The startup restore
/// has nevertheless already run through [`run_startup_sequence`].
pub(super) async fn run_boot_with_shutdown_restore<Output>(
    boot: impl Future<Output = Output>,
    restore_stale_gamma: impl FnOnce(),
) -> Output {
    let output = boot.await;
    restore_stale_gamma();
    output
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dormant_displays::gamma_breadcrumb::GammaBreadcrumb;
    use dormantd::gamma_recovery::{self, GammaSystemRestore};

    use super::*;

    #[derive(Clone, Default)]
    struct Trace(Arc<Mutex<Vec<&'static str>>>);

    impl Trace {
        fn push(&self, event: &'static str) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }

        fn events(&self) -> Vec<&'static str> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    struct TraceRestore(Trace);

    impl GammaSystemRestore for TraceRestore {
        fn restore_all(&self) -> Result<(), String> {
            self.0.push("gamma-restore");
            Ok(())
        }
    }

    fn startup_inputs(dir: &tempfile::TempDir) -> StartupInputs {
        StartupInputs {
            config_path: dir.path().join("config.toml"),
            creds_path: dir.path().join("credentials.toml"),
            state_dir: dir.path().join("state"),
            strictness: Strictness::Strict,
        }
    }

    #[test]
    fn startup_restore_precedes_config_prepare_and_every_terminal_path() {
        let paths = [
            ("logging-init-failed", false),
            ("runtime-init-failed", false),
            ("lock-failed", true),
            ("build-failed", true),
            ("started-clean", true),
            ("started-run-loop-failed", true),
            ("boot-error", true),
        ];

        for (terminal, reaches_boot) in paths {
            let dir = tempfile::tempdir().expect("tempdir");
            let inputs = startup_inputs(&dir);
            let breadcrumb = GammaBreadcrumb::new(&inputs.state_dir);
            breadcrumb.add_selector("cg:panel").expect("write marker");
            let marker_path = breadcrumb.path();
            let trace = Trace::default();
            let restore = TraceRestore(trace.clone());
            let actual = run_startup_sequence(
                inputs,
                {
                    let trace = trace.clone();
                    move |state_dir| {
                        gamma_recovery::restore_stale_breadcrumb(state_dir, &restore, "startup");
                        if !marker_path.exists() {
                            trace.push("marker-clear");
                        }
                    }
                },
                {
                    let trace = trace.clone();
                    move |_, _| {
                        trace.push("config-load");
                    }
                },
                {
                    let trace = trace.clone();
                    move |_, &()| {
                        trace.push("boot-guard-prepare");
                    }
                },
                {
                    let trace = trace.clone();
                    move |_, (), (), ()| {
                        if reaches_boot {
                            trace.push("boot");
                        }
                        trace.push(terminal);
                        terminal
                    }
                },
            );

            let mut expected = vec![
                "gamma-restore",
                "marker-clear",
                "config-load",
                "boot-guard-prepare",
            ];
            if reaches_boot {
                expected.push("boot");
            }
            expected.push(terminal);
            assert_eq!(actual, terminal);
            assert_eq!(trace.events(), expected, "terminal path: {terminal}");
        }
    }

    #[tokio::test]
    async fn shutdown_restore_follows_every_boot_verdict_path() {
        let paths = [
            "lock-failed",
            "build-failed",
            "started-clean",
            "started-run-loop-failed",
            "boot-error",
        ];

        for terminal in paths {
            let dir = tempfile::tempdir().expect("tempdir");
            let breadcrumb = GammaBreadcrumb::new(dir.path());
            breadcrumb.add_selector("cg:panel").expect("write marker");
            let trace = Trace::default();
            let restore = TraceRestore(trace.clone());

            let actual = run_boot_with_shutdown_restore(
                {
                    let trace = trace.clone();
                    async move {
                        trace.push(terminal);
                        trace.push("run-complete");
                        terminal
                    }
                },
                {
                    let trace = trace.clone();
                    move || {
                        gamma_recovery::restore_stale_breadcrumb(dir.path(), &restore, "shutdown");
                        if !breadcrumb.exists() {
                            trace.push("marker-clear");
                        }
                    }
                },
            )
            .await;

            assert_eq!(actual, terminal);
            assert_eq!(
                trace.events(),
                [terminal, "run-complete", "gamma-restore", "marker-clear"],
                "boot verdict path: {terminal}",
            );
        }
    }
}
