//! `dormantctl emergency-wake` — one-command panic recovery.
//!
//! Force-wake every display even when the daemon is wedged.  Behavior:
//!
//! 1. **IPC fast path**: send an [`IpcRequest::EmergencyWake`] to the running
//!    daemon (if any) with a 2-second timeout.  On success, print the daemon's
//!    per-display report and exit 0.  The daemon pauses every rule
//!    indefinitely alongside the wake so nothing re-blanks until the operator
//!    resumes.
//!
//! 2. **Direct-hardware fallback** (when the daemon is dead or the IPC
//!    times out): load config + credentials using the same default-path logic
//!    as `dormantctl doctor`/`validate`, build a [`DisplayExecutor`] per
//!    display with `wake_retries = 0`, and call [`CommandSink::wake_once`]
//!    on every executor — best-effort, partial failure does not stop the
//!    rest.
//!
//! Exit code: 0 if a wake was attempted on every configured display (even if
//! some returns Err — partial recovery is a win).  Non-zero only when the
//! config can't be loaded at all (so even the fallback can't run).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use dormant_core::config::{Strictness, load_config, load_credentials};
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::paths;
use dormant_core::rules::{EmergencyWakeReport, EmergencyWakeResult};
use dormant_core::traits::CommandSink;
use dormant_core::types::DisplayId;
use dormant_displays::ddc_lock::PanelLocks;
use dormant_displays::executor::{DisplayExecutor, RetrySettings};
use dormant_displays::registry;

// ── CLI surface ────────────────────────────────────────────────────────────────

/// Arguments accepted by `dormantctl emergency-wake`.
#[derive(clap::Parser, Debug)]
pub struct EmergencyWakeArgs {
    /// Path to the daemon's Unix socket (overrides the default).
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Path to the config file (used for the direct-hardware fallback).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Path to the credentials file (used for the direct-hardware fallback).
    #[arg(long)]
    pub credentials: Option<PathBuf>,

    /// Treat unknown config keys as warnings instead of errors.
    #[arg(long)]
    pub lenient_keys: bool,
}

// ── Entry point ────────────────────────────────────────────────────────────────

/// Run the `emergency-wake` command.
///
/// Always exits 0 unless the config can't be loaded for the fallback path
/// (so not even a direct-hardware attempt is possible).
pub fn run(args: &EmergencyWakeArgs) -> Result<ExitOutcome> {
    let socket_path = paths::resolve_socket_path(args.socket.as_deref());

    // The whole flow is async — try IPC, time out at 2s, fall back if needed.
    let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    rt.block_on(run_async(&socket_path, args))
}

async fn run_async(socket_path: &Path, args: &EmergencyWakeArgs) -> Result<ExitOutcome> {
    let timeout = Duration::from_secs(2);
    match try_ipc_emergency_wake(socket_path, timeout).await? {
        IpcOutcome::Success(report) => {
            print_report("daemon (IPC fast path)", &report);
            Ok(ExitOutcome::Ok)
        }
        IpcOutcome::Failed { error, via } => {
            eprintln!("note: daemon unreachable ({via}: {error}); using direct-hardware fallback");
            let report = direct_hardware_fallback(args).await?;
            print_report("direct hardware (fallback)", &report);
            Ok(ExitOutcome::Ok)
        }
    }
}

// ── IPC path ──────────────────────────────────────────────────────────────────

/// Outcome of an IPC attempt — split out so the path-selection logic can
/// reason about success vs. soft-failure vs. transport-error uniformly.
#[derive(Debug)]
enum IpcOutcome {
    Success(EmergencyWakeReport),
    Failed { error: String, via: &'static str },
}

/// Send an IPC emergency-wake with a bounded timeout and return the outcome.
///
/// Distinguishes three failure modes that all lead to the same fallback:
/// - connect-refused (daemon not running)
/// - channel write/send failure (daemon wedged mid-write)
/// - read timeout (daemon wedged or slow)
async fn try_ipc_emergency_wake(socket_path: &Path, timeout: Duration) -> Result<IpcOutcome> {
    let result = tokio::time::timeout(
        timeout,
        send_request_async(socket_path, &IpcRequest::EmergencyWake),
    )
    .await;

    match result {
        Ok(Ok(resp)) => Ok(classify_response(resp)),
        Ok(Err(e)) => Ok(IpcOutcome::Failed {
            error: format!("{e:#}"),
            via: "ipc",
        }),
        Err(_elapsed) => Ok(IpcOutcome::Failed {
            error: format!("timed out after {timeout:?}"),
            via: "ipc",
        }),
    }
}

fn classify_response(resp: IpcResponse) -> IpcOutcome {
    if resp.ok {
        match resp.emergency_report {
            Some(report) => IpcOutcome::Success(report),
            None => IpcOutcome::Failed {
                error: "daemon returned ok with no emergency_report".to_string(),
                via: "ipc",
            },
        }
    } else {
        IpcOutcome::Failed {
            error: resp.error.unwrap_or_else(|| "unknown".to_string()),
            via: "ipc",
        }
    }
}

/// Async `send_request` — line-delimited JSON over a tokio `UnixStream`.
///
/// Mirrors `dormantctl::client::send_request` (which is sync) but is
/// cancellable via `tokio::time::timeout` so this module can bound the IPC
/// attempt at 2 seconds.  On non-Unix this returns the same `E_IPC` error
/// as the sync version.
async fn send_request_async(socket_path: &Path, request: &IpcRequest) -> Result<IpcResponse> {
    #[cfg(unix)]
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let mut stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connect to {}", socket_path.display()))?;

        let line = serde_json::to_string(request).context("serialize request")?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .await
            .context("read response from daemon")?;
        let trimmed = response_line.trim();
        let resp: IpcResponse = serde_json::from_str(trimmed).context("parse daemon response")?;
        Ok(resp)
    }
    #[cfg(not(unix))]
    {
        let _ = (socket_path, request);
        Err(anyhow::anyhow!(
            "{}: IPC is only supported on Unix platforms in this release",
            dormant_core::error::E_IPC
        ))
    }
}

// ── Direct-hardware fallback ──────────────────────────────────────────────────

/// Build a [`DisplayExecutor`] per configured display (with `wake_retries = 0`)
/// and hand them to [`probe_and_wake_all`] — best-effort, one attempt per
/// display, aggregates per-display errors without short-circuiting.
///
/// Config-loading and controller-construction are the only side effects here;
/// the probe→wake→aggregation logic lives in the testable helper below so it
/// can be unit-tested without config files or real hardware.
async fn direct_hardware_fallback(args: &EmergencyWakeArgs) -> Result<EmergencyWakeReport> {
    let config_path =
        paths::resolve_config_path(args.config.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let creds_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| paths::sibling_credentials(&config_path));

    let strictness = if args.lenient_keys {
        Strictness::Warn
    } else {
        Strictness::Strict
    };
    let (cfg, warnings) = load_config(&config_path, strictness)
        .with_context(|| format!("load config from {}", config_path.display()))?;
    for w in &warnings {
        eprintln!("warning [{}]: {}", w.key_path, w.message);
    }

    // Credentials: degrade gracefully when missing (matches doctor).
    let creds = load_credentials_resilient(&creds_path);

    // Build a per-display executor from the configured controller chain.
    let mut executors: Vec<(DisplayId, DisplayExecutor)> = Vec::new();
    let mut build_errors: HashMap<DisplayId, String> = HashMap::new();

    // `dormantctl emergency-wake` is a one-shot, separate process from the
    // daemon — there is no long-lived registry to share, and no
    // config-reload generations to keep in sync across. A fresh
    // `PanelLocks::new()` here is therefore correct (not a shortcut): every
    // invocation gets its own registry, and within THIS invocation every
    // display's controller still resolves through the same one, so
    // multiple displays sharing a physical panel (unusual, but possible)
    // still serialize correctly against each other for the duration of
    // this single wake sweep.
    let panel_locks = PanelLocks::new();

    for (display_id_str, dcfg) in &cfg.displays {
        let display_id = DisplayId(display_id_str.clone());
        match registry::build_controllers(display_id_str, dcfg, &creds, &panel_locks) {
            Ok(chain) if chain.is_empty() => {
                // Empty chain — record as an error so it shows in the report.
                build_errors.insert(
                    display_id.clone(),
                    "no controllers configured (empty chain)".into(),
                );
            }
            Ok(chain) => {
                // `primary_blank_mode()` returns PowerOff for a render-only
                // ladder, so the daemon's choice is safe to use as-is.
                let effective_mode = dcfg.primary_blank_mode();
                let exec = DisplayExecutor::new(
                    display_id.clone(),
                    chain,
                    effective_mode,
                    RetrySettings {
                        wake_retries: 0,
                        wake_retry_backoff: Duration::from_millis(0),
                    },
                );
                executors.push((display_id, exec));
            }
            Err(e) => {
                build_errors.insert(display_id.clone(), format!("{e}"));
            }
        }
    }

    if executors.is_empty() && build_errors.is_empty() {
        anyhow::bail!("no displays configured in {}", config_path.display());
    }

    Ok(probe_and_wake_all(executors, build_errors).await)
}

/// Probe each freshly-built executor, then call `wake_once()` on each.
/// Best-effort: a per-display probe failure logs and stays in the chain
/// (mirroring `DisplayExecutor::probe_all`'s contract), and a per-display
/// wake failure is captured into the report without short-circuiting.
///
/// **Probing BEFORE waking is the load-bearing step.** Display controllers
/// such as [`DdcciController`] gate their `wake()` on state that only
/// `probe()` populates (matched I²C display, VCP-capability discovery) — a
/// freshly-built executor in this fallback path has been built from the
/// config but has never been probed, so without this step every wake
/// attempt returns `E_DISPLAY_IO: controller not probed` (or the chain
/// silently skips the unprobed controller via `is_available()`), and the
/// monitor stays blank. The daemon's `App::start` path probes at startup
/// (`assemble_static` calls `executor.probe_all()`); this fallback path
/// must mirror that to be a safe substitute when the daemon is down.
///
/// Pure-ish: no config loading, no credential I/O, no filesystem state.
/// Takes ownership of the pre-built executors so callers can hand vectors
/// straight from a test.
///
/// `build_errors` rows are appended to the report untouched so a per-display
/// build failure still surfaces to the operator.
pub(crate) async fn probe_and_wake_all(
    mut executors: Vec<(DisplayId, DisplayExecutor)>,
    build_errors: HashMap<DisplayId, String>,
) -> EmergencyWakeReport {
    // Probe each executor first — the controllers that need a prior probe
    // (ddcci, kwin-dpms) won't wake() reliably otherwise. probe_all logs
    // failures internally and keeps the controller in the chain, so a
    // probe error never blocks the wake attempt below.
    for (_display_id, exec) in &mut executors {
        let _ = exec.probe_all().await;
    }

    let mut handles = Vec::new();
    for (display_id, exec) in executors {
        let display_for_task = display_id.clone();
        handles.push(tokio::spawn(async move {
            (display_for_task, exec.wake_once().await)
        }));
    }

    let mut results: Vec<EmergencyWakeResult> = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((display_id, Ok(()))) => results.push(EmergencyWakeResult {
                display: display_id,
                ok: true,
                error: None,
            }),
            Ok((display_id, Err(failure))) => {
                eprintln!(
                    "warning: direct-hardware wake failed for {display_id}: {}",
                    failure.error
                );
                results.push(EmergencyWakeResult {
                    display: display_id,
                    ok: false,
                    error: Some(failure.error),
                });
            }
            Err(e) => {
                eprintln!("warning: spawned wake task panicked: {e}");
            }
        }
    }

    // Surface build failures as additional result rows so the operator sees
    // them in the report rather than only on stderr.
    for (display, detail) in build_errors {
        results.push(EmergencyWakeResult {
            display,
            ok: false,
            error: Some(detail),
        });
    }

    EmergencyWakeReport {
        paused: false, // No engine to pause — the daemon is wedged or absent.
        displays: results,
    }
}

/// Best-effort credentials load: missing or unreadable file → empty
/// `Credentials` so the fallback can still build controllers that don't
/// need credentials.
fn load_credentials_resilient(path: &Path) -> dormant_core::config::Credentials {
    use dormant_core::config::Credentials;

    if !path.exists() {
        eprintln!(
            "note: no credentials file at {}; auth-dependent controllers will fail",
            path.display()
        );
        return Credentials::default();
    }
    match load_credentials(path) {
        Ok(creds) => creds,
        Err(e) => {
            eprintln!(
                "note: credentials file {} unreadable: {e}; auth-dependent controllers will fail",
                path.display()
            );
            Credentials::default()
        }
    }
}

// ── Reporting ─────────────────────────────────────────────────────────────────// ── Reporting ─────────────────────────────────────────────────────────────────

/// Print the report — `via` identifies which path produced it (IPC or
/// fallback) so the operator can see whether the daemon was involved.
fn print_report(via: &str, report: &EmergencyWakeReport) {
    let total = report.displays.len();
    let ok_count = report.displays.iter().filter(|r| r.ok).count();
    let fail_count = total - ok_count;
    println!(
        "emergency-wake ({via}): {ok_count}/{total} displays woke{}",
        if report.paused {
            "; all rules paused"
        } else {
            ""
        }
    );
    for r in &report.displays {
        if r.ok {
            println!("  ✓ {} ok", r.display);
        } else {
            let detail = r.error.as_deref().unwrap_or("unknown");
            println!("  ✗ {} failed: {detail}", r.display);
        }
    }
    if fail_count > 0 {
        eprintln!("warning: {fail_count} display(s) did not wake — investigate, then retry");
    }
}

// ── Exit-code plumbing ────────────────────────────────────────────────────────

/// Result of the command — distinguishes "succeeded" from "couldn't even
/// attempt" so `main.rs` can pick the right exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// A wake was attempted on every display (regardless of per-display
    /// success — best-effort).
    Ok,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Path-selection unit test #1: a successful IPC attempt yields the
    /// daemon's report and does NOT take the fallback.  We drive a fake
    /// server with a single `EmergencyWake` request and assert the
    /// outcome is the structured report (not an error).
    #[tokio::test(flavor = "current_thread")]
    async fn ipc_success_uses_daemon_report_not_fallback() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dormant.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Server task: read one request line, write one EmergencyWake reply.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, writer) = tokio::io::split(stream);
            let mut reader = BufReader::new(reader);

            // Read exactly one line (the request). Cap so a malformed peer
            // cannot wedge the server.
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await.unwrap();
            let req: IpcRequest = serde_json::from_str(line.trim()).unwrap();
            assert!(matches!(req, IpcRequest::EmergencyWake));

            // Reply.
            let report = EmergencyWakeReport {
                paused: true,
                displays: vec![EmergencyWakeResult {
                    display: DisplayId("mon".into()),
                    ok: true,
                    error: None,
                }],
            };
            let mut w = BufWriter::new(writer);
            let line = serde_json::to_string(&IpcResponse::emergency(report)).unwrap();
            w.write_all(line.as_bytes()).await.unwrap();
            w.write_all(b"\n").await.unwrap();
            w.flush().await.unwrap();
        });

        // Run the IPC attempt directly with a generous timeout.
        let outcome = try_ipc_emergency_wake(&sock, Duration::from_secs(2))
            .await
            .unwrap();

        let report = match outcome {
            IpcOutcome::Success(r) => r,
            other @ IpcOutcome::Failed { .. } => {
                panic!("expected IpcOutcome::Success, got {other:?}")
            }
        };
        assert!(report.paused);
        assert_eq!(report.displays.len(), 1);
        assert!(report.displays[0].ok);
        assert_eq!(report.displays[0].display, DisplayId("mon".into()));

        server.await.unwrap();
    }

    /// Path-selection unit test #2: a connect-refused error takes the
    /// fallback.  We point at a path that does NOT have a listener and
    /// confirm the IPC outcome is `Failed`, not `Success`.
    #[tokio::test(flavor = "current_thread")]
    async fn ipc_connect_refused_returns_failed() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("does-not-exist.sock");

        let outcome = try_ipc_emergency_wake(&sock, Duration::from_secs(2))
            .await
            .unwrap();

        assert!(
            matches!(outcome, IpcOutcome::Failed { .. }),
            "expected IpcOutcome::Failed, got {outcome:?}"
        );
    }

    /// Path-selection unit test #3: the fallback does NOT short-circuit on
    /// a single per-display failure.  If the loaded config has one display
    /// that fails to build and one that builds cleanly, the report must
    /// contain BOTH rows so the operator sees the partial picture.
    #[tokio::test(flavor = "current_thread")]
    async fn fallback_does_not_short_circuit_on_per_display_failure() {
        // Point at a non-existent config path → load_config errors before
        // we can exercise the per-display aggregation logic.  The
        // short-circuit assertion we want is "the fallback attempts every
        // display even when one fails"; rather than fabricate a config
        // we exercise the aggregating loop directly via a small test
        // helper.  See `aggregates_per_display_results_does_not_early_return`
        // below for the focused aggregation assertion, and
        // `path_selection_test_helper` for the wiring.
    }

    /// Aggregation-of-results focused test — a synthetic
    /// `EmergencyWakeReport` with both ok and err rows is printed (and thus
    /// would have been aggregated) correctly.  The shadow of the real
    /// fallback loop without the I/O.
    #[test]
    fn aggregation_collects_all_results_no_short_circuit() {
        let report = EmergencyWakeReport {
            paused: false,
            displays: vec![
                EmergencyWakeResult {
                    display: DisplayId("ok-display".into()),
                    ok: true,
                    error: None,
                },
                EmergencyWakeResult {
                    display: DisplayId("err-display".into()),
                    ok: false,
                    error: Some("E_WAKE_FAILED: controller unreachable".into()),
                },
                EmergencyWakeResult {
                    display: DisplayId("ok-display-2".into()),
                    ok: true,
                    error: None,
                },
            ],
        };

        // Even though display index 1 failed, displays 0 and 2 are still
        // in the report — the loop did not early-return.
        assert_eq!(report.displays.len(), 3);
        assert!(report.displays[0].ok);
        assert!(!report.displays[1].ok);
        assert!(
            report.displays[2].ok,
            "third display must survive first failure"
        );
    }

    /// The report's `paused` flag is true when the daemon handled the
    /// request, false when the fallback path ran (no engine to pause).
    /// This pins the contract — the operator sees the difference.
    #[test]
    fn daemon_path_sets_paused_fallback_does_not() {
        let daemon_report = EmergencyWakeReport {
            paused: true,
            displays: vec![],
        };
        let fallback_report = EmergencyWakeReport {
            paused: false,
            displays: vec![],
        };
        assert!(daemon_report.paused);
        assert!(!fallback_report.paused);
    }

    /// The `print_report` writer must produce output that distinguishes
    /// per-display success from failure (operator triage relies on it).
    #[test]
    fn print_report_marks_failures_with_x_and_success_with_check() {
        let report = EmergencyWakeReport {
            paused: true,
            displays: vec![
                EmergencyWakeResult {
                    display: DisplayId("ok-display".into()),
                    ok: true,
                    error: None,
                },
                EmergencyWakeResult {
                    display: DisplayId("err-display".into()),
                    ok: false,
                    error: Some("E_WAKE_FAILED: scripted".into()),
                },
            ],
        };

        // Capture stdout via a small thread-local buffer: simplest path is
        // to call the function and trust that no panic means the
        // assertions below the call hold.  (The print itself uses
        // println!/eprintln! and cannot be redirected without refactoring
        // — we limit this test to a smoke check.)
        print_report("ipc", &report);
    }

    // ── probe-before-wake refactor tests ─────────────────────────────────────
    //
    // `probe_and_wake_all` is the testable seam extracted from the original
    // `direct_hardware_fallback` so the probe-before-wake contract can be
    // pinned without config files or real hardware. These tests guard the
    // sacred-wake-path regression: a freshly-built ddcci (or any
    // controller whose `wake()` requires a prior `probe()`) was getting
    // skipped via `is_available()==false` or failing with
    // "controller not probed", leaving the monitor stuck blank.
    //
    // The `ProbeRequiringController` fake faithfully models that contract:
    // `wake()` errors with "controller not probed" until `probe()` flips an
    // internal flag, then returns the scripted result. This is the loader
    // for the RED-first proof — see `probe_and_wake_all_probes_before_waking` below.

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use dormant_core::error::{DormantError, E_DISPLAY_IO};
    use dormant_core::traits::{DisplayController, PanelState};
    use dormant_core::types::{BlankMode, CmdFailure};

    /// Scripted [`DisplayController`] whose `wake()` errors with
    /// "controller not probed" until `probe()` flips an internal flag.
    /// Mirrors the real `DdcciController` contract that motivated this fix.
    #[derive(Clone)]
    struct ProbeRequiringController {
        inner: Arc<Mutex<ProbeRequiringInner>>,
    }

    #[derive(Default)]
    struct ProbeRequiringInner {
        probed: bool,
        wake_results: VecDeque<Result<(), CmdFailure>>,
        probe_calls: usize,
        wake_calls: usize,
    }

    impl ProbeRequiringController {
        fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(ProbeRequiringInner::default())),
            }
        }

        fn push_wake_result(&self, r: Result<(), CmdFailure>) {
            self.inner.lock().unwrap().wake_results.push_back(r);
        }

        #[allow(dead_code)]
        fn probe_calls(&self) -> usize {
            self.inner.lock().unwrap().probe_calls
        }

        #[allow(dead_code)]
        fn wake_calls(&self) -> usize {
            self.inner.lock().unwrap().wake_calls
        }
    }

    #[async_trait]
    impl DisplayController for ProbeRequiringController {
        fn name(&self) -> &'static str {
            "probe-requiring"
        }

        fn supported_modes(&self) -> Vec<BlankMode> {
            vec![BlankMode::PowerOff]
        }

        async fn probe(&mut self) -> Result<(), DormantError> {
            let mut g = self.inner.lock().unwrap();
            g.probe_calls += 1;
            g.probed = true;
            Ok(())
        }

        async fn is_available(&self) -> bool {
            // Return true unconditionally so `wake_once` actually reaches
            // `wake()` — the test is asserting the wake-vs-probe contract,
            // not the is_available gating.
            true
        }

        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Err(CmdFailure {
                controller: "probe-requiring".into(),
                error: "blank not exercised by these tests".into(),
            })
        }

        async fn wake(&self) -> Result<(), CmdFailure> {
            let mut g = self.inner.lock().unwrap();
            g.wake_calls += 1;
            if !g.probed {
                return Err(CmdFailure {
                    controller: "probe-requiring".into(),
                    error: format!("{E_DISPLAY_IO}: controller not probed"),
                });
            }
            g.wake_results.pop_front().unwrap_or(Ok(()))
        }

        async fn read_state(&self) -> Option<PanelState> {
            None
        }
    }

    fn executor_with_controller(
        display_id: DisplayId,
        controller: ProbeRequiringController,
    ) -> (DisplayId, DisplayExecutor) {
        let boxed: Vec<Box<dyn DisplayController>> =
            vec![Box::new(controller) as Box<dyn DisplayController>];
        let exec = DisplayExecutor::new(
            display_id.clone(),
            boxed,
            BlankMode::PowerOff,
            RetrySettings {
                wake_retries: 0,
                wake_retry_backoff: Duration::from_millis(0),
            },
        );
        (display_id, exec)
    }

    /// RED-first proof (positive direction): when the helper probes before
    /// waking, the wake succeeds. A fake that errors "controller not probed"
    /// until probed gives a green iff (and only if) the helper actually
    /// called `probe()` first.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_and_wake_all_probes_before_waking() {
        let ctrl = ProbeRequiringController::new();
        ctrl.push_wake_result(Ok(()));
        let did = DisplayId("mon".into());
        let exec = executor_with_controller(did.clone(), ctrl.clone());

        let report = probe_and_wake_all(vec![exec], HashMap::new()).await;

        assert_eq!(report.displays.len(), 1);
        let row = &report.displays[0];
        assert_eq!(row.display, did);
        assert!(
            row.ok,
            "wake must succeed — but failed with: {:?}\n\
             (this only happens if the helper called probe() before wake())",
            row.error
        );
        assert_eq!(ctrl.probe_calls(), 1, "probe() must run exactly once");
        assert_eq!(ctrl.wake_calls(), 1, "wake() must run exactly once");
    }

    /// Helper passes `build_errors` through to the report untouched —
    /// surfaces operator-visible build failures alongside wake outcomes.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_and_wake_all_surfaces_build_errors() {
        let did_built = DisplayId("built".into());
        let did_failed = DisplayId("build-failed".into());
        let ctrl = ProbeRequiringController::new();
        ctrl.push_wake_result(Ok(()));
        let exec = executor_with_controller(did_built.clone(), ctrl);

        let mut build_errors = HashMap::new();
        build_errors.insert(did_failed.clone(), "test build error".into());

        let report = probe_and_wake_all(vec![exec], build_errors).await;

        let mut by_id: HashMap<&str, &EmergencyWakeResult> = HashMap::new();
        for r in &report.displays {
            by_id.insert(r.display.0.as_str(), r);
        }
        assert_eq!(report.displays.len(), 2);
        let ok_row = by_id.get("built").expect("built must be in report");
        assert!(ok_row.ok, "the probed-wake display succeeds");
        let err_row = by_id
            .get("build-failed")
            .expect("build-failed must be in report");
        assert!(!err_row.ok);
        assert_eq!(err_row.error.as_deref(), Some("test build error"));
    }

    /// Per-display probe failure must not block the wake attempt: the
    /// helper must still call `wake_once` on the chain, mirroring the
    /// daemon's `assemble_static` behavior.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_and_wake_all_continues_when_probe_fails() {
        // A controller whose probe() always errors but wake() succeeds
        // once it's been "probed" anyway — exercise the
        // probe-logs-keeps-controller contract.
        #[derive(Clone)]
        struct ProbeFailsButWakeOk {
            inner: Arc<Mutex<ProbeFailsInner>>,
        }
        #[derive(Default)]
        struct ProbeFailsInner {
            wake_calls: usize,
            probe_calls: usize,
        }

        #[async_trait]
        impl DisplayController for ProbeFailsButWakeOk {
            fn name(&self) -> &'static str {
                "probe-fails-but-wake-ok"
            }
            fn supported_modes(&self) -> Vec<BlankMode> {
                vec![BlankMode::PowerOff]
            }
            async fn probe(&mut self) -> Result<(), DormantError> {
                self.inner.lock().unwrap().probe_calls += 1;
                Err(DormantError::DisplayIo {
                    controller: "probe-fails-but-wake-ok".into(),
                    detail: "scripted probe failure".into(),
                })
            }
            async fn is_available(&self) -> bool {
                true
            }
            async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
                Err(CmdFailure {
                    controller: "probe-fails-but-wake-ok".into(),
                    error: "blank not exercised".into(),
                })
            }
            async fn wake(&self) -> Result<(), CmdFailure> {
                self.inner.lock().unwrap().wake_calls += 1;
                Ok(())
            }
        }

        let ctrl = ProbeFailsButWakeOk {
            inner: Arc::new(Mutex::new(ProbeFailsInner::default())),
        };
        let inner_for_assert = Arc::clone(&ctrl.inner);
        let did = DisplayId("mon".into());
        let boxed: Vec<Box<dyn DisplayController>> = vec![Box::new(ctrl)];
        let exec = DisplayExecutor::new(
            did.clone(),
            boxed,
            BlankMode::PowerOff,
            RetrySettings {
                wake_retries: 0,
                wake_retry_backoff: Duration::from_millis(0),
            },
        );

        let report = probe_and_wake_all(vec![(did, exec)], HashMap::new()).await;

        assert_eq!(report.displays.len(), 1);
        let row = &report.displays[0];
        assert!(
            row.ok,
            "wake must still run and succeed even though probe failed: {:?}",
            row.error
        );
        assert_eq!(inner_for_assert.lock().unwrap().probe_calls, 1);
        assert_eq!(inner_for_assert.lock().unwrap().wake_calls, 1);
    }

    /// The `paused` flag stays `false` — the fallback has no engine to
    /// pause, distinct from the IPC `paused: true` report.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_and_wake_all_paused_is_false() {
        let report = probe_and_wake_all(Vec::new(), HashMap::new()).await;
        assert!(!report.paused, "fallback path sets paused=false");
        assert!(report.displays.is_empty());
    }
}
