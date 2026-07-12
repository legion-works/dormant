//! Integration tests for `dormantd::boot` (T5, spec §5.1): `prepare()` +
//! `boot()` driven end to end with real tempdir state, real (per-test)
//! sockets, and the production `App::build`/`App::start` path — `boot()` is
//! the only call site allowed to reach `App::start` in production, so these
//! tests exercise it directly rather than through `App::build_with_sources`
//! fakes.
//!
//! **Isolation (spec Global Constraints + plan P5):** every seeded config
//! sets `daemon.socket_path` to a per-test tempdir path (the
//! `ipc_roundtrip.rs`/`daemon_smoke.rs` tempdir-socket pattern) — real IPC
//! is left ENABLED (never `.disable_ipc()`) because `boot()` drives
//! `App::start`, which binds it for real; the tempdir path is what isolates
//! each test from the others and from a real daemon, not a `disable_ipc()`
//! call that `boot()` has no seam for anyway. `state_dir`/`lock_path` are
//! likewise per-test tempdirs threaded through `BootInputs` directly — no
//! `XDG_STATE_HOME`/`XDG_RUNTIME_DIR` env manipulation, so no cross-test
//! lock is needed for any test except the lock-failure one, which shares a
//! `lock_path` ON PURPOSE.
//!
//! Sensors use the REAL `dormant_sensors` registry (via `App::build`, not
//! `build_with_sources`): `MqttSource::new` is a plain struct constructor
//! (see `dormant-sensors/src/registry.rs`) — no network I/O happens until a
//! background task later polls it, so an unreachable `tcp://localhost:1883`
//! broker never blocks `App::build`/`App::start`.
//!
//! **Parallel-execution safety:** `install_capture_subscriber` installs a
//! process-global subscriber whose buffer is shared by every test in this
//! binary. The three tests that emit or assert on `config_rollback_boot`
//! (`bad_config_with_lkg_immediate_rollback`,
//! `bad_config_and_bad_lkg_build_failed`,
//! `counted_rollback_never_attempts_chosen`) serialize via a shared
//! [`capture_lock`] — the same pattern `daemon_smoke.rs` uses for its
//! count-sensitive capture tests. Every other test in this file emits only
//! event names that do not collide with the assertions of those three.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dormant_core::config::Strictness;
use dormant_core::rules::{ControlMsg, StateSnapshot};
use dormantd::app::AppHandle;
use dormantd::boot::{self, BootOutcome};
use dormantd::boot_guard::{
    self, BootInputs, CrashLoopState, DeferredEvent, Fingerprint, StartEntry, fingerprint_bytes,
};
use dormantd::sd_notify::SdNotify;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tracing_subscriber::fmt::MakeWriter;

// ── Log capture (mirrors `daemon_smoke.rs`'s `install_capture_subscriber`) ──

static CAPTURE: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

fn install_capture_subscriber() {
    CAPTURE.get_or_init(|| Mutex::new(Vec::new()));
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(CaptureWriter)
            .finish(),
    );
}

#[derive(Clone)]
struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE
            .get()
            .expect("capture subscriber not installed")
            .lock()
            .unwrap()
            .write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter
    }
}

fn drain_capture() -> String {
    CAPTURE.get().map_or(String::new(), |buf| {
        let mut guard = buf.lock().unwrap();
        let s = String::from_utf8(guard.clone()).unwrap_or_default();
        guard.clear();
        s
    })
}

/// Serializes the three tests that emit or read the `config_rollback_boot`
/// event — they share a process-global capture buffer (see
/// [`install_capture_subscriber`]), and a parallel sibling test that also
/// calls `boot()` with a bad-config+LKG config can emit the event into the
/// buffer between a capture-reader's `drain_capture()` and its assertion.
///
/// The poison-recovery idiom matches `daemon_smoke.rs`'s
/// `CAPTURE_COUNT_LOCK` — a panicking test must not wedge the rest of the
/// binary.
static CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_lock() -> &'static Mutex<()> {
    CAPTURE_LOCK.get_or_init(|| Mutex::new(()))
}

// ── Config fixtures ──────────────────────────────────────────────────────

fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    std::fs::write(dir.join(name), contents).unwrap();
    dir.join(name)
}

/// A valid one-display, one-rule config — real `command` controller, real
/// `mqtt` sensor (unreachable broker; safe, see module docs), `daemon.
/// socket_path` pinned to a per-test tempdir path.
fn good_config(socket_path: &Path) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"
socket_path = "{sock}"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "true"
wake_command = "true"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "0s"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        sock = socket_path.display(),
    )
}

/// Same shape as [`good_config`] but the rule references a zone that does
/// not exist — `load_config` still parses/reads this fine (a real
/// fingerprint-able, byte-comparable file), but `validate()` rejects it, so
/// `App::build` fails. Distinguishable bytes from `good_config` (extra
/// comment line) so the two never collide fingerprint-wise.
fn bad_config(socket_path: &Path) -> String {
    format!(
        r#"# bad: rule references an undefined zone
config_version = 1
[daemon]
startup_holdoff = "0s"
socket_path = "{sock}"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "true"
wake_command = "true"
modes = ["power_off"]

[rules.r]
zone = "does-not-exist"
displays = ["mon"]
grace_period = "0s"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        sock = socket_path.display(),
    )
}

// ── Harness ──────────────────────────────────────────────────────────────

struct Harness {
    dir: TempDir,
    state_dir: PathBuf,
    lock_path: PathBuf,
    creds_path: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let state_dir = dir.path().join("state");
        let lock_path = dir.path().join("dormant.lock");
        let creds_path = dir.path().join("credentials.toml");
        Self {
            dir,
            state_dir,
            lock_path,
            creds_path,
        }
    }

    fn socket_path(&self) -> PathBuf {
        // A SEPARATE tempdir-ish subpath for the socket, mirroring
        // `ipc_roundtrip.rs`/`daemon_smoke.rs`'s "never the default path"
        // isolation note (kept short — abstract/unix socket path length
        // limits).
        self.dir.path().join("d.sock")
    }

    fn inputs(&self) -> BootInputs {
        BootInputs {
            creds_path: self.creds_path.clone(),
            strictness: Strictness::Strict,
            state_dir: self.state_dir.clone(),
            lock_path: self.lock_path.clone(),
            sd_notify: SdNotify::from_env(),
        }
    }

    fn write_good(&self) -> PathBuf {
        write_file(
            self.dir.path(),
            "config.toml",
            &good_config(&self.socket_path()),
        )
    }

    fn write_bad(&self) -> PathBuf {
        write_file(
            self.dir.path(),
            "config.toml",
            &bad_config(&self.socket_path()),
        )
    }

    fn write_lkg(&self, contents: &str) -> PathBuf {
        std::fs::create_dir_all(&self.state_dir).unwrap();
        let path = self.state_dir.join("last-known-good.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Seed `crash-loop.json` directly with a synthetic state (synthetic
    /// past timestamps for the "counted"/"sticky"/"quiet-retry" cases —
    /// `boot_guard`'s own atomic-write helpers are `pub(crate)`, so this
    /// test binary (an external crate) writes the plain JSON shape itself;
    /// `prepare`'s own `load_crash_loop_state` tolerates any valid decode).
    fn seed_crash_loop(&self, state: &CrashLoopState) {
        std::fs::create_dir_all(&self.state_dir).unwrap();
        let raw = serde_json::to_string_pretty(state).unwrap();
        std::fs::write(self.state_dir.join("crash-loop.json"), raw).unwrap();
    }
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn entry(epoch_s: u64, fp: Fingerprint, nonce: u64) -> StartEntry {
    StartEntry {
        epoch_s,
        fingerprint: fp,
        nonce,
    }
}

/// Fetch the live `pending_reload` message off a running app via
/// `ControlMsg::Snapshot` (`daemon_smoke`'s `snapshot_with_retry` pattern,
/// single-shot — no reload generation-swap window to retry across here).
async fn pending_reload_of(handle: &AppHandle) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    handle
        .control_sender()
        .send(ControlMsg::Snapshot(tx))
        .await
        .expect("send snapshot request");
    let snap: StateSnapshot = rx.await.expect("recv snapshot");
    snap.pending_reload
}

async fn shutdown(handle: AppHandle, join: tokio::task::JoinHandle<()>) {
    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
}

// ── Tests ────────────────────────────────────────────────────────────────

/// Bad chosen config, differing LKG present → immediate rollback: LKG used,
/// pending banner set, `config_rollback_boot` logged directly by `boot()`
/// (not deferred — this failure is discovered live, after `prepare` already
/// returned `Proceed`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes the three config_rollback_boot-sensitive tests and is \
              always released promptly at test end — see the lock's doc comment"
)]
async fn bad_config_with_lkg_immediate_rollback() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();
    drain_capture();
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let bad_fp = fingerprint_bytes(&bad_bytes);
    let lkg_path = h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(plan.chosen_config, bad, "prepare never validates — Proceed");

    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    match outcome {
        BootOutcome::Started {
            handle,
            join,
            used_config,
            rolled_back,
        } => {
            assert_eq!(used_config, lkg_path);
            assert!(rolled_back);
            let pending = pending_reload_of(&handle).await;
            assert!(
                pending
                    .as_deref()
                    .is_some_and(|m| m.contains("rolled back to last-known-good")),
                "got {pending:?}"
            );
            shutdown(handle, join).await;
        }
        BootOutcome::LockFailed | BootOutcome::BuildFailed(_) => panic!("expected Started"),
    }

    let log = drain_capture();
    assert!(
        log.contains("config_rollback_boot"),
        "boot() must log config_rollback_boot directly, got: {log}"
    );

    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(state.rollback_active, "boot()'s own write must set this");
    assert_eq!(
        state.rolled_back_from,
        Some(bad_fp),
        "boot()'s own immediate-rollback write must pin the failed (bad) config's fingerprint"
    );
}

/// Bad chosen config, no LKG at all → `BuildFailed` (regression pin: today's
/// "no fallback" behavior, unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_config_no_lkg_build_failed() {
    let h = Harness::new();
    let bad = h.write_bad();

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    assert!(
        matches!(outcome, BootOutcome::BuildFailed(_)),
        "no LKG must never roll back"
    );
}

/// Bad chosen config AND a bad (unbuildable) LKG → `BuildFailed` (spec
/// §5.1 point 3's "if THAT also fails" branch).
///
/// This test EMITS `config_rollback_boot` via the immediate-rollback branch
/// (the first build fails, the LKG also fails) — it is serialized under
/// [`capture_lock`] against the two capture-reader tests so its event cannot
/// contaminate a parallel assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes the three config_rollback_boot-sensitive tests and is \
              always released promptly at test end — see the lock's doc comment"
)]
async fn bad_config_and_bad_lkg_build_failed() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = Harness::new();
    let bad = h.write_bad();
    // An LKG with DIFFERENT, but ALSO invalid, bytes.
    h.write_lkg("config_version = 1\n[rules.x]\nzone = \"also-missing\"\ndisplays = []\n");

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    assert!(matches!(outcome, BootOutcome::BuildFailed(_)));
}

/// A counted crash-loop verdict (3 same-fingerprint starts, differing LKG)
/// rolls back BEFORE ever attempting the chosen (bad) config — pinned by
/// the ABSENCE of `boot()`'s own `config_rollback_boot` log line (that line
/// only fires on the immediate-rollback branch, which is never entered here
/// because `App::build(plan.chosen_config)` — the LKG, per `prepare`'s
/// verdict — succeeds on the first try).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes the three config_rollback_boot-sensitive tests and is \
              always released promptly at test end — see the lock's doc comment"
)]
async fn counted_rollback_never_attempts_chosen() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();
    drain_capture();
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let fp = fingerprint_bytes(&bad_bytes);
    let now = now_epoch_s();
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 60, fp, 101), entry(now - 30, fp, 102)],
        rollback_active: false,
        rolled_back_from: None,
    });
    let lkg_path = h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(
        plan.chosen_config, lkg_path,
        "prepare must already choose the LKG for a counted RollBack verdict"
    );

    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    match outcome {
        BootOutcome::Started {
            handle,
            join,
            used_config,
            rolled_back,
        } => {
            assert_eq!(used_config, lkg_path);
            assert!(rolled_back);
            shutdown(handle, join).await;
        }
        BootOutcome::LockFailed => panic!("expected Started, got LockFailed"),
        BootOutcome::BuildFailed(msg) => panic!("expected Started, got BuildFailed({msg})"),
    }

    let log = drain_capture();
    assert!(
        !log.contains("config validation failed at boot"),
        "boot()'s own immediate-rollback branch must never fire for a counted verdict, got: {log}"
    );
}

/// Sticky `ContinueRollback`: LKG used again, `config_rollback_continued`
/// deferred (prepare-level, unemitted here — that's main.rs's job), pending
/// banner re-parked by `boot()` from `plan.pending_message`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sticky_continue_rollback_reparks_banner() {
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let fp = fingerprint_bytes(&bad_bytes);
    let now = now_epoch_s();
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 30, fp, 201)],
        rollback_active: true,
        rolled_back_from: Some(fp),
    });
    let lkg_path = h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(plan.chosen_config, lkg_path);
    assert_eq!(plan.deferred_events, vec![DeferredEvent::RollbackContinued]);
    assert!(plan.pending_message.is_some());

    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    let BootOutcome::Started {
        handle,
        join,
        used_config,
        rolled_back,
    } = outcome
    else {
        panic!("expected Started");
    };
    assert_eq!(used_config, lkg_path);
    assert!(rolled_back);
    let pending = pending_reload_of(&handle).await;
    assert!(
        pending
            .as_deref()
            .is_some_and(|m| m.contains("sticky rollback")),
        "got {pending:?}"
    );
    shutdown(handle, join).await;
}

/// LKG missing mid-storm: the chosen (original) config is attempted, state
/// is disarmed/cleared, `lkg_missing_rollback_disarmed` deferred.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lkg_missing_mid_storm_disarms_and_attempts_chosen() {
    let h = Harness::new();
    let good = h.write_good();
    let good_bytes = std::fs::read(&good).unwrap();
    let fp = fingerprint_bytes(&good_bytes);
    let now = now_epoch_s();
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 30, fp, 301)],
        rollback_active: true,
        rolled_back_from: Some(fp),
    });
    // No LKG file written at all.

    let plan = boot_guard::prepare(&good, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(plan.chosen_config, good, "must attempt the ORIGINAL config");
    assert_eq!(
        plan.deferred_events,
        vec![DeferredEvent::LkgMissingRollbackDisarmed]
    );

    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    let BootOutcome::Started {
        handle,
        join,
        used_config,
        rolled_back,
    } = outcome
    else {
        panic!("expected Started");
    };
    assert_eq!(used_config, good);
    assert!(!rolled_back);
    shutdown(handle, join).await;

    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(!state.rollback_active, "must be disarmed");
}

/// `watchdog.lkg_rollback_enabled = false` suppresses ONLY the counted
/// rollback path — a genuine 3-same-fingerprint-in-window pattern with a
/// differing LKG still Proceeds with the chosen (original) config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lkg_rollback_disabled_suppresses_counted_rollback() {
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let fp = fingerprint_bytes(&bad_bytes);
    let now = now_epoch_s();
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 60, fp, 401), entry(now - 30, fp, 402)],
        rollback_active: false,
        rolled_back_from: None,
    });
    h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, false);
    assert_eq!(
        plan.chosen_config, bad,
        "gate=false must suppress the counted RollBack verdict"
    );
}

/// Quiet-period retry: the previous non-discounted start is older than
/// `CRASH_LOOP_WINDOW`, bytes unchanged → `Proceed` with the original
/// config, `config_rollback_retry` deferred, pending banner re-parked by
/// `boot()` from `plan.pending_message`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_retry_reparks_banner() {
    let h = Harness::new();
    let good = h.write_good();
    let good_bytes = std::fs::read(&good).unwrap();
    let fp = fingerprint_bytes(&good_bytes);
    let now = now_epoch_s();
    // > CRASH_LOOP_WINDOW (6m) old, bytes unchanged, sticky conditions also
    // technically hold — quiet-retry must still win (spec §5.2 step 2).
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 400, fp, 501)],
        rollback_active: true,
        rolled_back_from: Some(fp),
    });
    h.write_lkg(
        "config_version = 1\n# a different, valid LKG\n[daemon]\nstartup_holdoff = \"0s\"\n",
    );

    let plan = boot_guard::prepare(&good, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(plan.chosen_config, good);
    assert!(matches!(
        plan.deferred_events.as_slice(),
        [DeferredEvent::RollbackRetry { .. }]
    ));

    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    let BootOutcome::Started {
        handle,
        join,
        used_config,
        rolled_back,
    } = outcome
    else {
        panic!("expected Started");
    };
    assert_eq!(used_config, good);
    assert!(!rolled_back);
    let pending = pending_reload_of(&handle).await;
    assert!(
        pending
            .as_deref()
            .is_some_and(|m| m.contains("retrying latest config after a quiet period")),
        "got {pending:?}"
    );
    shutdown(handle, join).await;
}

/// A discounted (loser-of-a-lock-race) start never counts toward the
/// crash-loop threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discounted_start_not_counted() {
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let fp = fingerprint_bytes(&bad_bytes);
    let now = now_epoch_s();
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 60, fp, 601), entry(now - 30, fp, 602)],
        rollback_active: false,
        rolled_back_from: None,
    });
    boot_guard::record_discount(&h.state_dir, 601);
    h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(
        plan.chosen_config, bad,
        "one discounted + one live prior + this boot = 2 live, below threshold"
    );
}

/// A fixed config (fingerprint differs from `rolled_back_from`) clears
/// `rollback_active` at the NEXT boot (F2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_config_clears_rollback_active() {
    let h = Harness::new();
    let good = h.write_good();
    // rolled_back_from is some OTHER (old, bad) fingerprint — never equal
    // to the fixed config's real bytes.
    let old_fp = fingerprint_bytes(b"old-bad-bytes-not-on-disk-anymore");
    let now = now_epoch_s();
    h.seed_crash_loop(&CrashLoopState {
        schema_version: 1,
        starts: vec![entry(now - 30, old_fp, 701)],
        rollback_active: true,
        rolled_back_from: Some(old_fp),
    });
    h.write_lkg("config_version = 1\n[daemon]\nstartup_holdoff = \"0s\"\n");

    let plan = boot_guard::prepare(&good, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    assert_eq!(plan.chosen_config, good);

    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    let BootOutcome::Started { handle, join, .. } = outcome else {
        panic!("expected Started");
    };
    shutdown(handle, join).await;

    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(!state.rollback_active, "F2: bytes-changed must clear");
}

/// A second `boot()` against an already-held lock returns `LockFailed` and
/// records a discount file for the LOSER's own nonce (P2 — never the
/// winner's).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_failure_discounts_the_losers_nonce() {
    let h = Harness::new();
    let good = h.write_good();

    let plan1 = boot_guard::prepare(&good, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let outcome1 = boot::boot(plan1, h.inputs()).await.expect("boot 1");
    let BootOutcome::Started {
        handle: handle1,
        join: join1,
        ..
    } = outcome1
    else {
        panic!("expected Started for the first boot()");
    };

    // Second `prepare()` against the SAME state_dir records a genuinely
    // distinct nonce (the process-lifetime counter in `generate_nonce`
    // guarantees this even within the same wall-clock second).
    let plan2 = boot_guard::prepare(&good, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let loser_nonce = plan2.nonce;
    assert_ne!(loser_nonce, 0);

    let outcome2 = boot::boot(plan2, h.inputs()).await.expect("boot 2");
    assert!(
        matches!(outcome2, BootOutcome::LockFailed),
        "second boot() against a held lock must fail"
    );

    assert!(
        h.state_dir.join(format!("discount-{loser_nonce}")).exists(),
        "the loser's own nonce must be discounted"
    );

    shutdown(handle1, join1).await;
}

/// A `Started` boot must send `READY=1` — and exactly one, and before any
/// `WATCHDOG=1` — by the time `boot()` returns (spec §6.2). Uses the
/// `SdNotify::from_socket_for_test` seam bound to a per-test tempdir
/// datagram socket (`daemon_smoke.rs`'s `fake_systemd_socket` pattern),
/// read back here with a short bounded recv.
///
/// Note this deliberately does NOT assert that no `WATCHDOG=1` ever
/// follows: `tokio::time::interval`'s first tick fires immediately (not
/// after a full period), so `run_loop`'s watchdog probe-arm can legitimately
/// send its own first `WATCHDOG=1` right behind `READY=1` once the engine
/// answers a healthy probe — `BootInputs` has no watchdog-interval override
/// seam (unlike `App::with_watchdog_interval`) to suppress that. What's
/// pinned is ORDER and COUNT: the very first datagram must be `READY=1`,
/// and a second datagram (if any lands in the short follow-up window) must
/// never be a second `READY=1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_sends_ready_before_any_watchdog_ping() {
    let h = Harness::new();
    let good = h.write_good();

    // Short name, mirroring `Harness::socket_path`'s own "never the default
    // path, keep it short" note (abstract/unix socket path length limits).
    let notify_path = h.dir.path().join("n.sock");
    let listener = std::os::unix::net::UnixDatagram::bind(&notify_path).unwrap();
    listener
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let addr = std::os::unix::net::SocketAddr::from_pathname(&notify_path).unwrap();

    let mut inputs = h.inputs();
    inputs.sd_notify = SdNotify::from_socket_for_test(&addr);

    let plan = boot_guard::prepare(&good, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let outcome = boot::boot(plan, inputs).await.expect("boot");
    let BootOutcome::Started { handle, join, .. } = outcome else {
        panic!("expected Started");
    };

    let mut buf = [0u8; 64];
    let (n, _) = listener
        .recv_from(&mut buf)
        .expect("expected a datagram by the time boot() returns");
    assert_eq!(
        &buf[..n],
        b"READY=1",
        "the first datagram boot() emits must be READY=1"
    );

    // Bounded follow-up: whatever (if anything) lands next must not be a
    // second READY=1. A timeout here (no second datagram at all) is also a
    // pass — the watchdog tick racing in is a possibility, not a guarantee.
    listener
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    if let Ok((n2, _)) = listener.recv_from(&mut buf) {
        assert_ne!(
            &buf[..n2],
            b"READY=1",
            "READY=1 must be sent exactly once per boot()"
        );
    }

    shutdown(handle, join).await;
}
