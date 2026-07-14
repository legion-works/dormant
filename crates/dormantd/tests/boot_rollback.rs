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
//! binary. Two overlapping reasons put a test under the shared
//! [`capture_lock`] (the same pattern `daemon_smoke.rs` uses for its
//! count-sensitive capture tests):
//!
//! - it emits or asserts on `config_rollback_boot`/"config validation
//!   failed at boot" — every test that boots from a bad config with a
//!   DIFFERING LKG present takes this immediate-rollback branch
//!   (`bad_config_with_lkg_immediate_rollback`,
//!   `bad_config_and_bad_lkg_build_failed`,
//!   `counted_rollback_never_attempts_chosen` — which asserts the literal
//!   is ABSENT — plus every rollback-recovery test below that boots the
//!   same way);
//! - it edits its own OPERATOR config file after boot while the real file
//!   watcher is installed: a genuine inotify event fires independently of
//!   whatever triggered the reload under test, logging
//!   `reload_trigger source="watcher"` into the SAME shared buffer
//!   (`accepted_reload_recovers_from_rollback_without_restart`,
//!   `rejected_reload_during_rollback_leaves_rollback_state_untouched`, and
//!   the Task 3 watcher-drill tests below).
//!
//! Every other test in this file emits only event names that do not
//! collide with any capture-reading assertion.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    // `with_ansi(false)`: the capture buffer is asserted on via plain
    // substring `.contains()` checks (Task 3's `source="watcher"` among
    // them) — ANSI color codes interposed between a field's key, `=`, and
    // value (tracing-subscriber's default when it can't detect the custom
    // `CaptureWriter` is a non-tty) would silently break any check that
    // spans a `key=value` boundary, even though single-token checks like
    // `contains("config_rollback_boot")` happened to survive it.
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(CaptureWriter)
            .with_ansi(false)
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

/// Bounded-poll [`drain_capture`] (accumulating what's drained across
/// polls, since each call clears the buffer) until `needle` appears or
/// `timeout` elapses — Task 3 §3's "bounded polling... never sleep-and-
/// assume" applied to the tracing capture rather than a `ControlMsg`
/// snapshot (`daemon_smoke.rs`'s `wait_for` is the same idiom over a
/// predicate).
async fn wait_for_capture(needle: &str, timeout: Duration) -> String {
    let start = Instant::now();
    let mut acc = String::new();
    loop {
        acc.push_str(&drain_capture());
        if acc.contains(needle) {
            return acc;
        }
        if start.elapsed() >= timeout {
            return acc;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

/// [`good_config`]'s shape with two additions the Task 3 watcher drill
/// needs: a fast `daemon.reload_debounce` (the real watcher-triggered
/// reload must fire promptly under this file's bounded polling — the
/// schema default of 500ms would push the drill toward its own timeout),
/// and an explicit `daemon.log_level` the caller controls — the LKG and
/// the fixed operator content pass DIFFERENT values so a live
/// `handle.config_watch()` read after recovery can prove WHICH bytes fed
/// the running generation, independent of the byte-identity check the
/// LKG-candidate seam already gives.
fn watcher_drill_config(socket_path: &Path, log_level: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"
reload_debounce = "10ms"
log_level = "{log_level}"
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

/// Replace `path`'s contents via a sibling temp file + rename — the
/// editor/`install(1)` pattern `reload::config_watcher`'s own module doc
/// names as the reason it watches the config file's PARENT DIRECTORY
/// rather than the file's inode (Task 3 §3: "make file replacement
/// watcher-safe").
fn replace_watched_file(path: &Path, contents: &str) {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, contents).unwrap();
    std::fs::rename(&tmp, path).unwrap();
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

            // Two explicit path roles (rollback-recovery plan, Task 1):
            // `used_config`/generation-0 assembly is the LKG (asserted
            // above), but the OPERATOR path retained by `AppHandle` (for
            // the watcher/Web UI/manual reload) must remain the original,
            // still-broken `bad` file — never silently swapped to the LKG
            // substitute.
            assert_eq!(
                handle.config_path(),
                bad.as_path(),
                "AppHandle must retain the operator config path across a rollback boot"
            );

            // Generation 0 itself must be assembled from the LKG's bytes,
            // not the broken operator bytes: `good_config`'s rule
            // references the real `office` zone, while `bad_config`'s
            // rule references a nonexistent `does-not-exist` zone — a
            // directly assertable, queryable fingerprint of WHICH source
            // fed `load_cfg_creds`/`assemble_static`.
            let live_cfg = handle.config_watch().borrow().clone();
            assert_eq!(
                live_cfg.rules.get("r").map(|r| r.zone.as_str()),
                Some("office"),
                "generation 0 must be assembled from LKG-derived config values"
            );

            // Coupling-hazard suppression (plan Task 1 §5): arming an
            // initial LKG candidate from the OPERATOR path during a
            // rollback boot would track the broken bytes just rolled away
            // from — once healthy + `stability_window` elapse, `lkg_tick`
            // would promote them and corrupt `last-known-good.toml`. No
            // candidate must be armed at all while `rollback_active`.
            assert!(
                !handle.lkg_candidate_observed().armed,
                "no LKG candidate may be armed after a rollback boot"
            );

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

/// Rollback-recovery plan Task 2 §3/§4/§6: an ACCEPTED reload from the
/// operator path while a boot-time rollback is active is the live-reload
/// sibling of `boot_guard`'s `ContinueRollback -> Proceed` transition. The
/// operator fixes the broken file (distinct valid bytes from the LKG, so
/// the reload's effect is distinguishable from the boot-time rollback's),
/// triggers a manual reload (mirrors `dormantctl reload`; the file-watcher
/// path is Task 3's job), and recovers WITHOUT a restart: the persisted
/// crash-loop state clears, the pending-reload banner clears, and exactly
/// one fresh LKG candidate is armed from the newly-accepted operator
/// bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes every test in this file that boots an immediate rollback \
              or edits its own watched operator config post-boot against the tests that assert on \
              config_rollback_boot / reload_trigger literals in the shared global capture buffer — \
              see the module doc comment"
)]
async fn accepted_reload_recovers_from_rollback_without_restart() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = Harness::new();
    let bad = h.write_bad();
    let lkg_path = h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
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
    assert_eq!(used_config, lkg_path, "generation 0 must boot from the LKG");
    assert!(rolled_back);

    // Pre-recovery sanity: banner set, no candidate armed (Task 1 §5 gate).
    assert!(
        pending_reload_of(&handle).await.is_some(),
        "pending banner must be set immediately after a rollback boot"
    );
    assert!(
        !handle.lkg_candidate_observed().armed,
        "no LKG candidate may be armed before recovery"
    );

    // The operator fixes the broken file — a DIFFERENT valid config from
    // the LKG's own bytes (a shorter `startup_holdoff`), so every
    // assertion below is provably fed by the fresh reload, not a residual
    // LKG-derived value.
    let fixed = good_config(&h.socket_path()).replacen(
        "startup_holdoff = \"0s\"",
        "startup_holdoff = \"0ms\"",
        1,
    );
    std::fs::write(&bad, &fixed).unwrap();

    let mut reloads = handle.subscribe_reload();
    assert!(handle.trigger_reload().await, "trigger_reload must send");
    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome within timeout")
        .expect("reload broadcast channel open");
    assert_eq!(
        outcome,
        dormantd::app::ReloadOutcome::Reloaded,
        "got {outcome:?}"
    );

    // The pending-reload banner clears on the newly-installed engine
    // (Task 2 §3: `ControlMsg::SetPendingReload(None)` before the
    // broadcast above).
    assert_eq!(
        pending_reload_of(&handle).await,
        None,
        "pending_reload banner must clear on an accepted recovery reload"
    );

    // Exactly one fresh LKG candidate armed from the accepted operator
    // bytes (Task 2 §6) — via the committed test-util seam, never the
    // 5-minute stability_window wait.
    let observed = handle.lkg_candidate_observed();
    assert!(
        observed.armed,
        "a fresh LKG candidate must be armed after recovery"
    );
    assert_eq!(observed.source, Some("reload"));
    assert_eq!(
        observed.bytes.as_deref(),
        Some(fixed.as_bytes()),
        "the fresh candidate must be armed from the newly-accepted operator bytes"
    );

    shutdown(handle, join).await;

    // The persisted crash-loop state clears (Task 2 §3/§4): both
    // `rollback_active` and `rolled_back_from`, atomically, via
    // `clear_rollback_after_reload`.
    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(
        !state.rollback_active,
        "rollback_active must clear after an accepted recovery reload"
    );
    assert_eq!(
        state.rolled_back_from, None,
        "rolled_back_from must clear after an accepted recovery reload"
    );
}

/// Rollback-recovery plan Task 2 §5: a REJECTED reload while rollback is
/// active must leave every piece of rollback state byte-for-byte
/// untouched — pending banner stays set, crash-loop JSON stays active, and
/// no candidate is armed. Sibling of the accepted-reload recovery test
/// above; this one edits the operator file with STILL-invalid bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes every test in this file that boots an immediate rollback \
              or edits its own watched operator config post-boot against the tests that assert on \
              config_rollback_boot / reload_trigger literals in the shared global capture buffer — \
              see the module doc comment"
)]
async fn rejected_reload_during_rollback_leaves_rollback_state_untouched() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let bad_fp = fingerprint_bytes(&bad_bytes);
    h.write_lkg(&good_config(&h.socket_path()));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    let BootOutcome::Started { handle, join, .. } = outcome else {
        panic!("expected Started");
    };

    // A second, still-broken edit (different bytes, still an undefined
    // zone reference) — `load_and_assemble` must reject it before ever
    // touching the running generation.
    let still_bad = bad_config(&h.socket_path()).replace("does-not-exist", "still-missing");
    std::fs::write(&bad, &still_bad).unwrap();

    let mut reloads = handle.subscribe_reload();
    assert!(handle.trigger_reload().await, "trigger_reload must send");
    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome within timeout")
        .expect("reload broadcast channel open");
    assert!(
        matches!(outcome, dormantd::app::ReloadOutcome::Rejected(_)),
        "got {outcome:?}"
    );

    // Banner remains set — a rejected reload's own `Err` arm re-parks it
    // with the NEW rejection detail (`Runner::reload`'s early-return `Err`
    // arm sends `SetPendingReload(Some(detail))` before returning), so it
    // is never cleared to `None` the way an ACCEPTED recovery reload does.
    assert!(
        pending_reload_of(&handle).await.is_some(),
        "pending banner must remain set after a rejected reload"
    );
    assert!(
        !handle.lkg_candidate_observed().armed,
        "no candidate may be armed by a rejected reload"
    );

    shutdown(handle, join).await;

    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(
        state.rollback_active,
        "rollback_active must NOT clear on a rejected reload"
    );
    assert_eq!(
        state.rolled_back_from,
        Some(bad_fp),
        "rolled_back_from must NOT change on a rejected reload"
    );
}

/// Rollback-recovery plan Task 3 §1: the full LKG drill through the REAL
/// file watcher, in-process — no LIVE-daemon drill (operator-gated,
/// morning work, out of scope here). Distinct from Task 2's own
/// `accepted_reload_recovers_from_rollback_without_restart`: this test
/// never calls `trigger_reload()` — the fixed operator bytes reach the
/// daemon only through a real filesystem event on the OPERATOR path (a
/// sibling-temp + rename, matching `reload::config_watcher`'s own watched-
/// directory reasoning), and the bounded-polled tracing capture proves the
/// watcher actually fired before the reload outcome is awaited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes every test in this file that boots an immediate rollback \
              or edits its own watched operator config post-boot against the tests that assert on \
              config_rollback_boot / reload_trigger literals in the shared global capture buffer — \
              see the module doc comment"
)]
async fn rollback_boot_watches_operator_path_and_reload_recovers_without_restart() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();
    drain_capture();

    let h = Harness::new();
    let bad = h.write_bad();
    let lkg_contents = watcher_drill_config(&h.socket_path(), "info");
    let lkg_path = h.write_lkg(&lkg_contents);

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
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
    assert_eq!(used_config, lkg_path, "generation 0 must boot from the LKG");
    assert!(rolled_back);

    // Pre-fix sanity: generation 0 is running the LKG's own bytes
    // (`log_level = "info"`, distinct from the fixed operator content
    // below), the pending-reload banner is set, and the crash-loop JSON
    // records an active rollback — all BEFORE the watcher ever fires.
    assert_eq!(
        handle.config_watch().borrow().daemon.log_level,
        "info",
        "generation 0 must be assembled from the LKG's own bytes"
    );
    assert!(
        pending_reload_of(&handle).await.is_some(),
        "pending banner must be set immediately after a rollback boot"
    );
    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(
        state.rollback_active,
        "crash-loop JSON must be active before the fix"
    );
    assert!(state.rolled_back_from.is_some());
    assert!(
        !handle.lkg_candidate_observed().armed,
        "no LKG candidate may be armed before recovery"
    );

    // The operator fixes the broken OPERATOR file — watcher-safe
    // replacement, a DIFFERENT `log_level` from the LKG's so the live
    // config assertion below is provably fed by these bytes. Subscribe to
    // reload outcomes BEFORE the edit: the watcher's own reload is async
    // and unprompted, unlike `trigger_reload()`'s directly-awaited send.
    let fixed = watcher_drill_config(&h.socket_path(), "debug");
    let mut reloads = handle.subscribe_reload();
    replace_watched_file(&bad, &fixed);

    // Bounded-poll the capture for the real watcher firing — proof this
    // reload was never manually triggered (no `trigger_reload()` call
    // anywhere in this test).
    let log = wait_for_capture(r#"source="watcher""#, Duration::from_secs(5)).await;
    assert!(
        log.contains("reload_trigger") && log.contains(r#"source="watcher""#),
        "expected a real watcher-driven reload_trigger, got: {log}"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome within timeout")
        .expect("reload broadcast channel open");
    assert_eq!(
        outcome,
        dormantd::app::ReloadOutcome::Reloaded,
        "got {outcome:?}"
    );

    assert_eq!(
        handle.config_watch().borrow().daemon.log_level,
        "debug",
        "live config watch must reflect the fixed OPERATOR value, not the LKG's"
    );
    assert_eq!(
        pending_reload_of(&handle).await,
        None,
        "pending_reload banner must clear on a watcher-driven recovery reload"
    );

    let observed = handle.lkg_candidate_observed();
    assert!(
        observed.armed,
        "a fresh LKG candidate must be armed after recovery"
    );
    assert_eq!(observed.source, Some("reload"));
    assert_eq!(
        observed.bytes.as_deref(),
        Some(fixed.as_bytes()),
        "exactly one candidate must be armed from the OPERATOR bytes"
    );

    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(
        !state.rollback_active,
        "rollback_active must clear after watcher-driven recovery"
    );
    assert_eq!(
        state.rolled_back_from, None,
        "rolled_back_from must clear after watcher-driven recovery"
    );

    shutdown(handle, join).await;
}

/// Rollback-recovery plan Task 3 §2's sibling rejected-reload assertion,
/// driven through the real watcher rather than `trigger_reload()` — the
/// same differentiator as the drill above, rather than duplicating Task
/// 2's own manual-trigger
/// `rejected_reload_during_rollback_leaves_rollback_state_untouched`. A
/// second, STILL invalid edit to the operator file (through the same
/// watcher-safe replacement) while a boot-time rollback is active must
/// leave every piece of rollback state untouched: the pending banner
/// stays set, the crash-loop JSON stays active, and no candidate is
/// armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_lock() serializes every test in this file that boots an immediate rollback \
              or edits its own watched operator config post-boot against the tests that assert on \
              config_rollback_boot / reload_trigger literals in the shared global capture buffer — \
              see the module doc comment"
)]
async fn rollback_boot_watcher_rejects_still_bad_operator_edit() {
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = Harness::new();
    let bad = h.write_bad();
    let bad_bytes = std::fs::read(&bad).unwrap();
    let bad_fp = fingerprint_bytes(&bad_bytes);
    h.write_lkg(&watcher_drill_config(&h.socket_path(), "info"));

    let plan = boot_guard::prepare(&bad, &h.creds_path, &h.state_dir, Strictness::Strict, true);
    let outcome = boot::boot(plan, h.inputs()).await.expect("boot");
    let BootOutcome::Started { handle, join, .. } = outcome else {
        panic!("expected Started");
    };

    // A second, still-broken edit (different bytes, still an undefined
    // zone reference) — delivered ONLY through the watcher, never
    // `trigger_reload()`.
    let still_bad = bad_config(&h.socket_path()).replace("does-not-exist", "still-missing");
    let mut reloads = handle.subscribe_reload();
    replace_watched_file(&bad, &still_bad);

    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome within timeout (watcher-triggered)")
        .expect("reload broadcast channel open");
    assert!(
        matches!(outcome, dormantd::app::ReloadOutcome::Rejected(_)),
        "got {outcome:?}"
    );

    assert!(
        pending_reload_of(&handle).await.is_some(),
        "pending banner must remain set after a rejected watcher-triggered reload"
    );
    assert!(
        !handle.lkg_candidate_observed().armed,
        "no candidate may be armed by a rejected reload"
    );

    shutdown(handle, join).await;

    let state: CrashLoopState = serde_json::from_str(
        &std::fs::read_to_string(h.state_dir.join("crash-loop.json")).unwrap(),
    )
    .unwrap();
    assert!(
        state.rollback_active,
        "rollback_active must NOT clear on a rejected watcher-triggered reload"
    );
    assert_eq!(
        state.rolled_back_from,
        Some(bad_fp),
        "rolled_back_from must NOT change on a rejected watcher-triggered reload"
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

            // Same two-path-role + coupling-hazard pins as
            // `bad_config_with_lkg_immediate_rollback`, for the COUNTED
            // rollback route (`prepare` chose the LKG up front, rather
            // than `boot()` discovering the failure live).
            assert_eq!(
                handle.config_path(),
                bad.as_path(),
                "AppHandle must retain the operator config path across a counted rollback boot"
            );
            let live_cfg = handle.config_watch().borrow().clone();
            assert_eq!(
                live_cfg.rules.get("r").map(|r| r.zone.as_str()),
                Some("office"),
                "generation 0 must be assembled from LKG-derived config values"
            );
            assert!(
                !handle.lkg_candidate_observed().armed,
                "no LKG candidate may be armed after a counted rollback boot"
            );

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
