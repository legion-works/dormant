//! Real-time daemon smoke tests.
//!
//! These wire a full [`App`] over a tempdir config with `command` display
//! controllers that append marker bytes to files, plus injected
//! [`FakeSensorSource`]s (no broker/serial in CI). Timings are tight but
//! real-clock; assertions are on ordering and presence, not exact ms.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use dormant_core::config::Strictness;
use dormant_core::config::schema::{Config, Credentials};
use dormant_core::fakes::FakeSensorSource;
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::observation::{DaemonObservation, GenerationId, ObservationHub, ReloadSource};
use dormant_core::rules::{ControlMsg, DaemonEvent, RollbackStatus, StateSnapshot};
use dormant_core::traits::SensorSource;
use dormant_core::types::{DisplayId, PresenceEvent, SensorId, SensorState, Timestamp};
use dormantd::app::{
    App, GenerationBarrierGate, ReloadLifecycleCapture, ReloadOutcome, validate_only,
};
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing_subscriber::fmt::MakeWriter;

// ── Helpers ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn recv_terminal_outcome_skips_stale_reloaded_receipts() {
    let (tx, mut reloads) = broadcast::channel(2);
    tx.send(ReloadOutcome::Reloaded)
        .expect("send stale reloaded receipt");
    tx.send(ReloadOutcome::Rejected("invalid config".into()))
        .expect("send rejected receipt");

    let outcome = recv_terminal_outcome(&mut reloads, Duration::from_secs(1)).await;

    assert_eq!(outcome, ReloadOutcome::Rejected("invalid config".into()));
}

// INTERIM (#92): superseded by correlated reload receipts (attempt ids) — see .opencode/ci-hardening/2026-07-18-direction.md PR 1.
/// Receives a rejected reload outcome while ignoring stale successful receipts.
///
/// After an invalid-config write, [`ReloadOutcome::Reloaded`] can only be a
/// stale receipt from the preceding valid reload's duplicate watcher/trigger
/// execution: invalid content cannot validate. A genuinely accepted invalid
/// reload would also update the config or credentials watch, which the callers
/// assert separately.
async fn recv_terminal_outcome(
    reloads: &mut broadcast::Receiver<ReloadOutcome>,
    timeout: Duration,
) -> ReloadOutcome {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut observed = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for Rejected reload outcome; observed: {observed:?}"
        );

        match tokio::time::timeout(remaining, reloads.recv()).await {
            Ok(Ok(ReloadOutcome::Rejected(detail))) => return ReloadOutcome::Rejected(detail),
            Ok(Ok(outcome)) => observed.push(outcome),
            Ok(Err(error)) => {
                // RecvError covers both Closed and Lagged — keep the message neutral.
                panic!(
                    "reload bus error while waiting for Rejected; observed: {observed:?}; error: {error}"
                );
            }
            Err(error) => {
                panic!(
                    "timed out waiting for Rejected reload outcome; observed: {observed:?}; error: {error}"
                );
            }
        }
    }
}

/// Resilient snapshot: retries across the reload generation-switch window.
///
/// After a successful reload the reload outcome is signalled before
/// `forward_ctl`'s watch switches to the new generation's sender.  A
/// [`ControlMsg::Snapshot`] sent in that window lands on the old (now
/// closed) sender and its embedded oneshot is dropped → `RecvError`.
/// Retrying until the new generation is serving reflects real IPC-client
/// behavior (issue #9 — a documented, accepted v1 limitation). The daemon
/// is behaving correctly; the test must tolerate the transient window.
async fn snapshot_with_retry(ctl: &mpsc::Sender<ControlMsg>) -> StateSnapshot {
    const ATTEMPTS: usize = 10;
    const SLEEP: Duration = Duration::from_millis(100);
    const RECV_TIMEOUT: Duration = Duration::from_secs(2);

    for remaining in (0..ATTEMPTS).rev() {
        let (tx, rx) = oneshot::channel();
        if ctl.send(ControlMsg::Snapshot(tx)).await.is_err() {
            tokio::time::sleep(SLEEP).await;
            continue;
        }
        match tokio::time::timeout(RECV_TIMEOUT, rx).await {
            Ok(Ok(snap)) => return snap,
            _ => {
                if remaining > 0 {
                    tokio::time::sleep(SLEEP).await;
                }
            }
        }
    }
    panic!("snapshot_with_retry: all {ATTEMPTS} attempts exhausted");
}

/// Poll `Status` snapshots (via [`snapshot_with_retry`], so it tolerates the
/// reload generation-swap window) until `display`'s `last_blank_failed`
/// equals `want` or `timeout` elapses.
async fn wait_for_last_blank_failed(
    ctl: &mpsc::Sender<ControlMsg>,
    display: &str,
    want: bool,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let snap = snapshot_with_retry(ctl).await;
        if let Some((_, d)) = snap.displays.iter().find(|(id, _)| id == display)
            && d.last_blank_failed == want
        {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll `Status` snapshots (via [`snapshot_with_retry`]) until `sensor`'s
/// `reported` bit equals `want` or `timeout` elapses.
async fn wait_for_sensor_reported(
    ctl: &mpsc::Sender<ControlMsg>,
    sensor: &str,
    want: bool,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let snap = snapshot_with_retry(ctl).await;
        if let Some(s) = snap.sensors.iter().find(|s| s.id == sensor)
            && s.reported == want
        {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll `Status` snapshots (via [`snapshot_with_retry`]) until `display`'s
/// `phase` equals `want` or `timeout` elapses. A command sent over the
/// control channel (`ForceBlank`, a reload's defensive wake, ...) writes its
/// marker byte before the engine has finished processing the command's
/// RESULT and landing the phase transition — a point-in-time snapshot taken
/// right after the marker appears can still observe the PRIOR phase (e.g.
/// "blanking" instead of "blanked"). Bounded-poll instead of asserting once.
async fn wait_for_phase(
    ctl: &mpsc::Sender<ControlMsg>,
    display: &str,
    want: &str,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let snap = snapshot_with_retry(ctl).await;
        if let Some((_, d)) = snap.displays.iter().find(|(id, _)| id == display)
            && d.phase == want
        {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll snapshots until a display's effective inhibition bit reaches `want`.
async fn wait_for_display_inhibited(
    ctl: &mpsc::Sender<ControlMsg>,
    display: &str,
    want: bool,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let snap = snapshot_with_retry(ctl).await;
        if let Some((_, display)) = snap.displays.iter().find(|(id, _)| id == display)
            && display.inhibited == want
        {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).expect("write file");
    path
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn count(path: &Path, ch: char) -> usize {
    read(path).chars().filter(|c| *c == ch).count()
}

fn ev(sensor: &str, state: SensorState) -> PresenceEvent {
    PresenceEvent::new(SensorId(sensor.into()), state, Timestamp::now())
}

/// A source factory that replays `script` for `sensor` on every (re)build.
fn fake_factory(
    id: &str,
    script: Vec<(Duration, PresenceEvent)>,
) -> impl Fn(&Config, &Credentials) -> anyhow::Result<Vec<Box<dyn SensorSource>>> + Send + Sync + 'static
{
    let template = FakeSensorSource {
        id: id.to_string(),
        script,
    };
    move |_cfg: &Config, _creds: &Credentials| {
        Ok(vec![Box::new(template.clone()) as Box<dyn SensorSource>])
    }
}

async fn wait_for<F: Fn() -> bool>(pred: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}

/// Receive the next observation matching `predicate`, retaining unrelated
/// diagnostics so a test awaits a transition instead of a scheduler delay.
async fn recv_observation(
    observations: &mut broadcast::Receiver<DaemonObservation>,
    timeout: Duration,
    label: &str,
    predicate: impl Fn(&DaemonObservation) -> bool,
) -> DaemonObservation {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {label}; saw {seen:?}"
        );

        match tokio::time::timeout(remaining, observations.recv()).await {
            Ok(Ok(observation)) if predicate(&observation) => return observation,
            Ok(Ok(observation)) => seen.push(observation),
            Ok(Err(error)) => {
                panic!("observation stream failed waiting for {label}: {error}; saw {seen:?}")
            }
            Err(error) => panic!("timed out waiting for {label}: {error}; saw {seen:?}"),
        }
    }
}

/// One-display `command`-controller config with tunable grace and marker path.
fn one_display_config(marker: &Path, grace: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

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
blank_command = "printf B >> '{m}'"
wake_command = "printf W >> '{m}'"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m = marker.display(),
        g = grace,
    )
}

async fn shutdown(handle: dormantd::app::AppHandle, join: tokio::task::JoinHandle<()>) {
    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
}

async fn start_coordinator_app(
    config_path: PathBuf,
    creds_path: PathBuf,
) -> (dormantd::app::AppHandle, tokio::task::JoinHandle<()>) {
    App::build_with_sources(
        config_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build coordinator app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .start()
    .await
    .expect("start coordinator app")
}

fn coordinator_config(marker: &Path, startup_holdoff: &str) -> String {
    one_display_config(marker, "1s").replace(
        "startup_holdoff = \"0s\"",
        &format!("startup_holdoff = \"{startup_holdoff}\"\nreload_debounce = \"100ms\""),
    )
}

/// Write a credentials file with correct 0o600 permissions (Unix).
fn write_credentials(dir: &Path, toml: &str) -> PathBuf {
    let path = dir.join("credentials.toml");
    fs::write(&path, toml).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

/// The global capture buffer, initialised once per process.
static CAPTURE: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

/// Install a global tracing subscriber that feeds into `CAPTURE`
/// (idempotent — `OnceLock` guarantees single init).
fn install_capture_subscriber() {
    CAPTURE.get_or_init(|| Mutex::new(Vec::new()));
    // Set global only once — ignore subsequent calls.
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(CaptureWriter)
            .finish(),
    );
}

/// A `MakeWriter` + `io::Write` that feeds into the global `OnceLock<Mutex<Vec<u8>>>`.
#[derive(Clone)]
struct CaptureWriter;

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        CAPTURE
            .get()
            .expect("capture subscriber not installed")
            .lock()
            .unwrap()
            .write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        CAPTURE
            .get()
            .expect("capture subscriber not installed")
            .lock()
            .unwrap()
            .flush()
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter
    }
}

/// Drain the capture buffer (clears it) and return its contents.
fn drain_capture() -> String {
    CAPTURE.get().map_or(String::new(), |buf| {
        let mut guard = buf.lock().unwrap();
        let s = String::from_utf8(guard.clone()).unwrap_or_default();
        guard.clear();
        s
    })
}

// ── Notify-sink isolation ───────────────────────────────────────────────────

use dormantd::notifier::{NotifySink, zbus_sink_was_constructed};

struct NoopNotifySink;

#[async_trait::async_trait]
impl NotifySink for NoopNotifySink {
    async fn notify(
        &self,
        _summary: &str,
        _body: &str,
        _urgency: u8,
        _replaces: u32,
    ) -> Result<u32, String> {
        Ok(0)
    }

    async fn close(&self, _id: u32) -> Result<(), String> {
        Ok(())
    }
}

/// Every smoke-test App construction site must route through this factory
/// unless the test explicitly opts into notification tracking with a
/// `RecordingSink`.  The factory is called once per `App::start` (a fresh
/// `Arc` each generation — the sink is stateless, so sharing would be safe
/// but adds no value).
fn noop_factory() -> std::sync::Arc<dyn NotifySink> {
    std::sync::Arc::new(NoopNotifySink)
}

// ── 1: blank then wake ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_blank_and_wake() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg = one_display_config(&marker, "300ms");
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml"); // absent → defaults

    let script = vec![
        (Duration::from_millis(0), ev("desk", SensorState::Present)),
        (Duration::from_millis(200), ev("desk", SensorState::Absent)),
        (
            Duration::from_millis(1300),
            ev("desk", SensorState::Present),
        ),
    ];
    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let ok = wait_for(
        || {
            let s = read(&marker);
            s.contains('B') && s.contains('W')
        },
        Duration::from_secs(3),
    )
    .await;
    let content = read(&marker);
    shutdown(handle, join).await;

    assert!(ok, "expected both blank and wake, got {content:?}");
    let b = content.find('B');
    let w = content.find('W');
    assert!(
        b.is_some() && w.is_some() && b < w,
        "expected B before W, got {content:?}"
    );
    assert!(
        !zbus_sink_was_constructed(),
        "ZbusSink must never be constructed by any smoke test — \
         every App construction site must inject a no-op notify sink"
    );
}

// ── 2: --validate-only exit codes ──────────────────────────────────────────────

#[test]
fn validate_only_exit_codes() {
    let dir = TempDir::new().unwrap();
    let creds = dir.path().join("credentials.toml");

    let bad = "config_version = 1\nbogus_top_key = 5\n";
    let bad_path = write_file(dir.path(), "bad.toml", bad);
    let report = validate_only(&bad_path, &creds, Strictness::Strict);
    assert_ne!(report.exit_code(), 0, "bad config must exit non-zero");
    let mut out = String::new();
    report.render(&mut out);
    assert!(
        out.contains("bogus_top_key"),
        "message must name the offending key: {out}"
    );

    let good = "config_version = 1\n";
    let good_path = write_file(dir.path(), "good.toml", good);
    let ok = validate_only(&good_path, &creds, Strictness::Strict);
    assert_eq!(ok.exit_code(), 0, "valid config exits zero");
}

// ── 3: reload swaps behavior ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn reload_swap() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // Absent shortly after (re)start → one blank per generation.
    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur under the 400ms grace"
    );

    // Rewrite with a much shorter grace and reload.
    fs::write(&cfg_path, one_display_config(&marker, "100ms")).unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    let reload_at = Instant::now();
    assert!(
        wait_for(|| count(&marker, 'B') >= 2, Duration::from_secs(3)).await,
        "post-reload generation should blank again"
    );
    assert!(
        reload_at.elapsed() < Duration::from_secs(2),
        "post-reload blank should land promptly under the shorter grace"
    );

    shutdown(handle, join).await;
}

// ── 4: rejected reload keeps old config ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_rejected_keeps_old() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "120ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur"
    );
    let before = count(&marker, 'B');

    // Broken config: unknown key rejected in strict mode.
    fs::write(&cfg_path, "config_version = 1\nnonsense_key = true\n").unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    match outcome {
        ReloadOutcome::Rejected(detail) => assert!(!detail.is_empty()),
        ReloadOutcome::Reloaded => panic!("expected Rejected, got Reloaded"),
    }

    // pending_reload must surface in a snapshot.
    let ctl = handle.control_sender();
    let (tx, rx) = oneshot::channel();
    ctl.send(ControlMsg::Snapshot(tx)).await.unwrap();
    let snap: StateSnapshot = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("snapshot in time")
        .expect("snapshot reply");
    assert!(
        snap.pending_reload.is_some(),
        "pending_reload should be set after a rejected reload"
    );

    // Old behavior persists: the untouched generation was never torn down, so
    // it still blanks. Drive a wake+blank cycle through the injected-events
    // seam to prove the live engine keeps working.
    let events = handle.events_sender();
    events.send(ev("desk", SensorState::Present)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    events.send(ev("desk", SensorState::Absent)).await.unwrap();
    assert!(
        wait_for(|| count(&marker, 'B') > before, Duration::from_secs(3)).await,
        "old config should keep blanking after a rejected reload"
    );

    shutdown(handle, join).await;
}

// ── 5: removed display gets a verified wake ─────────────────────────────────────

fn two_display_config(
    m1: &Path,
    m2: &Path,
    wake2: &str,
    include_display: bool,
    include_in_rule: bool,
) -> String {
    let mon2_block = if include_display {
        format!(
            r#"
[displays.mon2]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m2}'"
wake_command = "{w2}"
modes = ["power_off"]
"#,
            m2 = m2.display(),
            w2 = wake2,
        )
    } else {
        String::new()
    };
    let displays = if include_in_rule {
        r#"["mon1", "mon2"]"#
    } else {
        r#"["mon1"]"#
    };
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon1]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m1}'"
wake_command = "printf W >> '{m1}'"
modes = ["power_off"]
{mon2_block}
[rules.r]
zone = "office"
displays = {displays}
grace_period = "120ms"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m1 = m1.display(),
        mon2_block = mon2_block,
        displays = displays,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn removed_display_verified_wake() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let m1 = dir.path().join("mon1");
    let m2 = dir.path().join("mon2");
    let wake2 = format!("printf W >> '{}'", m2.display());
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &two_display_config(&m1, &m2, &wake2, true, true),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    // Both displays blank.
    assert!(
        wait_for(
            || count(&m1, 'B') >= 1 && count(&m2, 'B') >= 1,
            Duration::from_secs(3)
        )
        .await,
        "both displays should blank before reload"
    );

    // Drop mon2 and reload; it must be woken after teardown via its old
    // executor before the reload can succeed.
    fs::write(
        &cfg_path,
        two_display_config(&m1, &m2, &wake2, false, false),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);
    assert!(
        read(&m2).contains('W'),
        "removed display must receive a verified wake, mon2={:?}",
        read(&m2)
    );

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn removed_display_wake_failure_aborts() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let m1 = dir.path().join("mon1");
    let m2 = dir.path().join("mon2");
    // mon2's wake command always fails.
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &two_display_config(&m1, &m2, "false", true, true),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory);
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(
            || count(&m1, 'B') >= 1 && count(&m2, 'B') >= 1,
            Duration::from_secs(3)
        )
        .await,
        "both displays should blank before reload"
    );

    fs::write(
        &cfg_path,
        two_display_config(&m1, &m2, "false", false, false),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome")
        .expect("reload bus open");
    match outcome {
        ReloadOutcome::Rejected(detail) => assert!(
            detail.contains("mon2"),
            "reject detail should name the un-wakeable display: {detail}"
        ),
        ReloadOutcome::Reloaded => panic!("expected Rejected on wake failure, got Reloaded"),
    }

    // pending_reload surfaces the failure.
    let ctl = handle.control_sender();
    let (tx, rx) = oneshot::channel();
    ctl.send(ControlMsg::Snapshot(tx)).await.unwrap();
    let snap: StateSnapshot = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("snapshot")
        .expect("snapshot reply");
    assert!(
        snap.pending_reload.is_some(),
        "pending_reload should be set after an aborted reload"
    );

    shutdown(handle, join).await;
}

// ── Retained dark display gets a defensive wake on reload ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn reload_defensive_wake_retained() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "150ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "display should blank before reload"
    );
    assert_eq!(count(&marker, 'W'), 0, "no wake before reload");

    // Reload while the display stays rule-controlled: the rebuilt machine
    // restarts Active, so the still-dark panel must get a defensive wake.
    fs::write(
        &cfg_path,
        format!(
            "{}\n# retain a distinct content revision",
            one_display_config(&marker, "150ms")
        ),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);
    assert!(
        wait_for(|| count(&marker, 'W') >= 1, Duration::from_secs(3)).await,
        "retained dark display must receive a defensive wake"
    );

    shutdown(handle, join).await;
}

// ── Display dropped from rules but kept in [displays] preserves phase ──────────
///
/// T4 behavior: a display kept in [displays] but dropped from every rule is
/// now manual-only — its phase is preserved across reload (no defensive wake).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn ruleless_display_preserves_phase_on_reload() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let m1 = dir.path().join("mon1");
    let m2 = dir.path().join("mon2");
    let wake2 = format!("printf W >> '{}'", m2.display());
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &two_display_config(&m1, &m2, &wake2, true, true),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(
            || count(&m1, 'B') >= 1 && count(&m2, 'B') >= 1,
            Duration::from_secs(3)
        )
        .await,
        "both displays should blank before reload"
    );
    // Ensure mon2's phase has actually landed at "blanked" before the
    // reload below — the B-marker byte (waited for above) is written by the
    // command executor before the engine finishes processing the blank
    // RESULT (same hazard as the manual-only-display tests, issue #94
    // sweep). Without this, the reload could race a still-"blanking" mon2
    // and the post-reload "preserved" assertion below would only be
    // preserving the wrong phase.
    let ctl = handle.control_sender();
    assert!(
        wait_for_phase(&ctl, "mon2", "blanked", Duration::from_secs(3)).await,
        "mon2's phase must settle to blanked before reload"
    );
    let m2_wake_before = count(&m2, 'W');
    let m2_blank_before = count(&m2, 'B');

    // Keep mon2 in [displays] but drop it from every rule — it becomes
    // manual-only.  T4 preserves its phase (blanked) across reload.
    fs::write(&cfg_path, two_display_config(&m1, &m2, &wake2, true, false)).unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    // mon2 should NOT be woken — its blanked phase is preserved.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        count(&m2, 'W'),
        m2_wake_before,
        "rule-less display must NOT be woken on reload (phase preserved), mon2={:?}",
        read(&m2)
    );
    assert_eq!(
        count(&m2, 'B'),
        m2_blank_before,
        "rule-less display must NOT re-blank on reload, mon2={:?}",
        read(&m2)
    );

    // Verify mon2 phase is still "blanked" in the snapshot.
    // Retry across the reload generation-switch window (issue #9).
    let snap = snapshot_with_retry(&handle.control_sender()).await;
    let m2_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon2")
        .map(|(_, ds)| ds);
    assert!(
        m2_snap.is_some(),
        "mon2 must still be present in snapshot after reload"
    );
    assert_eq!(
        m2_snap.unwrap().phase,
        "blanked",
        "rule-less display must preserve blanked phase after reload"
    );

    shutdown(handle, join).await;
}

// ── 8: config_watch updates on successful reload only ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::too_many_lines,
    reason = "the test keeps the complete reload observation sequence adjacent to its config mutation"
)]
async fn config_watch_updates_on_successful_reload_only() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let observations = ObservationHub::new(64);
    let mut observation_rx = observations.subscribe();
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_observation_hub(observations)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let mut config_watch = handle.config_watch();
    let initial_holdoff = config_watch.borrow_and_update().daemon.startup_holdoff;
    assert_eq!(initial_holdoff, Duration::from_secs(0));

    // The marker proves controller execution; the observation below proves
    // the owning state-machine transition.
    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur"
    );
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "rule display blanking to blanked",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(0),
                    rule_id: Some(rule_id),
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Blanking,
                    new_phase: dormant_core::state_machine::Phase::Blanked,
                } if rule_id.0 == "r" && display_id.0 == "mon"
            )
        },
    )
    .await;

    // Write a valid config with a different startup_holdoff and reload.
    let modified = one_display_config(&marker, "400ms")
        .replace("startup_holdoff = \"0s\"", "startup_holdoff = \"5s\"");
    fs::write(&cfg_path, &modified).unwrap();
    let (request_id, receipt) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("control reload receipt");
    assert!(receipt.request_ids.contains(&request_id));
    assert_eq!(receipt.outcome, ReloadOutcome::Reloaded);
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation zero drained",
        |observation| {
            matches!(
                observation,
                DaemonObservation::GenerationDrained {
                    generation: GenerationId(0)
                }
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation one started",
        |observation| {
            matches!(
                observation,
                DaemonObservation::GenerationStarted {
                    generation: GenerationId(1)
                }
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "reload completed for generation one",
        |observation| {
            matches!(
                observation,
                DaemonObservation::ReloadCompleted(receipt)
                    if receipt.generation == GenerationId(1)
                        && receipt.outcome == ReloadOutcome::Reloaded
            )
        },
    )
    .await;
    assert_eq!(
        config_watch.borrow_and_update().daemon.startup_holdoff,
        Duration::from_secs(5),
        "config watch must reflect successful reload"
    );

    // Now trigger a rejected reload — the watch must NOT change.
    fs::write(&cfg_path, "config_version = 1\nbogus = true\n").unwrap();
    let (rejected_request_id, rejected) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("rejected control reload receipt");
    assert!(rejected.request_ids.contains(&rejected_request_id));
    assert!(matches!(rejected.outcome, ReloadOutcome::Rejected(_)));

    // The receipt resolves only after this request has completed, so the
    // watch's change bit is a deterministic post-reload assertion.
    assert!(
        !config_watch
            .has_changed()
            .expect("config watch sender stays open"),
        "config watch must NOT update after a rejected reload"
    );

    shutdown(handle, join).await;
}

// ── 9: creds_watch updates on successful reload only ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn creds_watch_updates_on_successful_reload_only() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = write_credentials(dir.path(), "ha_token = \"initial\"");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    let mut creds_watch = handle.creds_watch();
    assert_eq!(
        creds_watch.borrow_and_update().ha_token,
        Some("initial".to_string())
    );

    // Wait for the initial blank so the run loop has fully settled.
    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur"
    );

    // Update the credentials and reload — watch must see the new value.
    let _creds_path = write_credentials(dir.path(), "ha_token = \"updated\"");
    let (request_id, receipt) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("control reload receipt");
    assert!(receipt.request_ids.contains(&request_id));
    assert_eq!(receipt.outcome, ReloadOutcome::Reloaded);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    // Bounded poll, not a single `changed()` — same stale-notification hazard
    // as `config_watch_updates_on_successful_reload_only` (issue #92/#94):
    // `changed()` resolves on ANY send since the last
    // `borrow_and_update()`/subscribe, so a prior pending notification can
    // resolve it before this reload's own `send_replace` has landed. Manual
    // loop (not the `wait_for` helper) because `borrow_and_update()` needs
    // `&mut`.
    let poll_start = Instant::now();
    let reflected = loop {
        if creds_watch.borrow_and_update().ha_token == Some("updated".to_string()) {
            break true;
        }
        if poll_start.elapsed() >= Duration::from_secs(2) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        reflected,
        "creds watch must reflect successful reload, got ha_token={:?}",
        creds_watch.borrow().ha_token
    );

    // Trigger a rejected reload — creds watch must NOT change.
    fs::write(&cfg_path, "config_version = 1\nbogus = true\n").unwrap();
    let (rejected_request_id, rejected) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("rejected control reload receipt");
    assert!(rejected.request_ids.contains(&rejected_request_id));
    assert!(matches!(rejected.outcome, ReloadOutcome::Rejected(_)));

    // The receipt resolves only after this request has completed, so the
    // watch's change bit is a deterministic post-reload assertion.
    assert!(
        !creds_watch
            .has_changed()
            .expect("credentials watch sender stays open"),
        "creds watch must NOT update after a rejected reload"
    );

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_batches_watcher_and_control_for_one_revision() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &coordinator_config(&marker, "0s"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (handle, join) = start_coordinator_app(cfg_path.clone(), creds_path).await;

    let (_, before) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("baseline receipt");
    fs::write(&cfg_path, coordinator_config(&marker, "1s")).unwrap();
    let (watcher, control) = tokio::join!(
        handle.request_reload_with_id(ReloadSource::Watcher),
        handle.request_reload_with_id(ReloadSource::Control),
    );
    let (watcher_id, watcher_receipt) = watcher.expect("watcher receipt");
    let (control_id, control_receipt) = control.expect("control receipt");

    assert_eq!(
        watcher_receipt, control_receipt,
        "one batch has one receipt"
    );
    assert!(watcher_receipt.request_ids.contains(&watcher_id));
    assert!(watcher_receipt.request_ids.contains(&control_id));
    assert!(watcher_receipt.sources.contains(&ReloadSource::Watcher));
    assert!(watcher_receipt.sources.contains(&ReloadSource::Control));
    assert!(!watcher_receipt.coalesced);
    assert_eq!(watcher_receipt.generation.0, before.generation.0 + 1);

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_batches_watcher_and_web_apply_for_one_revision() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &coordinator_config(&marker, "0s"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (handle, join) = start_coordinator_app(cfg_path.clone(), creds_path).await;

    fs::write(&cfg_path, coordinator_config(&marker, "2s")).unwrap();
    let (watcher, web_apply) = tokio::join!(
        handle.request_reload_with_id(ReloadSource::Watcher),
        handle.request_reload_with_id(ReloadSource::WebApply),
    );
    let (watcher_id, watcher_receipt) = watcher.expect("watcher receipt");
    let (web_id, web_receipt) = web_apply.expect("web-apply receipt");

    assert_eq!(
        watcher_receipt, web_receipt,
        "web apply joins the watcher batch"
    );
    assert!(web_receipt.request_ids.contains(&watcher_id));
    assert!(web_receipt.request_ids.contains(&web_id));
    assert!(web_receipt.sources.contains(&ReloadSource::Watcher));
    assert!(web_receipt.sources.contains(&ReloadSource::WebApply));
    assert!(!web_receipt.coalesced);

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_late_watcher_for_unchanged_revision_is_noop() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &coordinator_config(&marker, "0s"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (handle, join) = start_coordinator_app(cfg_path, creds_path).await;

    let (_, first) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("first receipt");
    let (watcher_id, late) = handle
        .request_reload_with_id(ReloadSource::Watcher)
        .await
        .expect("late watcher receipt");

    assert!(late.request_ids.contains(&watcher_id));
    assert!(late.coalesced);
    assert_eq!(late.outcome, ReloadOutcome::Reloaded);
    assert_eq!(
        late.generation, first.generation,
        "no generation was installed"
    );
    assert_eq!(late.applied_revision, first.applied_revision);

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_credentials_only_reload_changes_credentials_revision() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &coordinator_config(&marker, "0s"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (handle, join) = start_coordinator_app(cfg_path, creds_path.clone()).await;

    let (_, before) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("baseline receipt");
    write_credentials(dir.path(), "# credentials-only change\n");
    let (request_id, receipt) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("credentials receipt");

    assert!(receipt.request_ids.contains(&request_id));
    assert_eq!(receipt.outcome, ReloadOutcome::Reloaded);
    assert!(!receipt.coalesced);
    assert_eq!(
        receipt.requested_revision.config,
        before.applied_revision.config
    );
    assert_ne!(
        receipt.requested_revision.credentials,
        before.applied_revision.credentials
    );

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_rejects_invalid_request_with_its_own_receipt() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &coordinator_config(&marker, "0s"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (handle, join) = start_coordinator_app(cfg_path.clone(), creds_path).await;

    fs::write(&cfg_path, coordinator_config(&marker, "1s")).unwrap();
    let (_, valid) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("valid receipt");
    assert_eq!(valid.outcome, ReloadOutcome::Reloaded);

    fs::write(&cfg_path, "config_version = 1\nbogus = true\n").unwrap();
    let (invalid_id, invalid) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("invalid receipt");

    // Replacing the private one-shot with a shared broadcast receive lets this
    // waiter observe `valid` above; its own ID makes that regression falsifiable.
    assert!(invalid.request_ids.contains(&invalid_id));
    assert!(matches!(invalid.outcome, ReloadOutcome::Rejected(_)));

    shutdown(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_reload_inputs_are_delivered_exactly_once() {
    let dir = TempDir::new().unwrap();
    let old_event_marker = dir.path().join("old-event-marker");
    let old_control_marker = dir.path().join("old-control-marker");
    let new_event_marker = dir.path().join("new-event-marker");
    let new_control_marker = dir.path().join("new-control-marker");
    let generation_swap_config = |event_marker: &Path, control_marker: &Path| {
        format!(
            "{}\n[displays.manual]\ncontrollers = [\"command\"]\nblank_mode = \"power_off\"\nblank_command = \"printf B >> '{}'\"\nwake_command = \"printf W >> '{}'\"\nmodes = [\"power_off\"]\n",
            one_display_config(event_marker, "0s"),
            control_marker.display(),
            control_marker.display(),
        )
    };
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &generation_swap_config(&old_event_marker, &old_control_marker),
    );
    let creds_path = dir.path().join("credentials.toml");
    let gate = GenerationBarrierGate::new();
    let (handle, join) = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .unwrap()
    .with_notify_sink_builder(noop_factory)
    .with_test_generation_barrier_gate(gate.clone())
    .disable_ipc()
    .start()
    .await
    .unwrap();

    fs::write(
        &cfg_path,
        generation_swap_config(&new_event_marker, &new_control_marker).replace(
            "startup_holdoff = \"0s\"",
            "startup_holdoff = \"0s\"\nreload_debounce = \"1ms\"",
        ),
    )
    .unwrap();

    let events = handle.events_sender();
    let controls = handle.control_sender();
    {
        let reload = handle.request_reload_with_id(ReloadSource::Control);
        tokio::pin!(reload);
        tokio::select! {
            _ = &mut reload => panic!("reload completed before reaching the old-generation drain"),
            result = tokio::time::timeout(Duration::from_secs(1), gate.wait_until_entered()) => {
                assert!(result.is_ok(), "reload did not pause at the old-generation drain");
            }
        }

        events.send(ev("desk", SensorState::Absent)).await.unwrap();
        controls
            .send(ControlMsg::ForceBlank(DisplayId("manual".into())))
            .await
            .unwrap();
        gate.release();

        let (_, receipt) = tokio::time::timeout(Duration::from_secs(5), reload)
            .await
            .expect("reload completes")
            .expect("reload receipt");
        assert_eq!(receipt.outcome, ReloadOutcome::Reloaded);
    }
    assert!(
        wait_for(
            || count(&new_event_marker, 'B') == 1 && count(&new_control_marker, 'B') == 1,
            Duration::from_secs(2),
        )
        .await
    );
    assert_eq!(
        count(&old_event_marker, 'B'),
        0,
        "event reached old generation"
    );
    assert_eq!(
        count(&old_control_marker, 'B'),
        0,
        "control reached old generation"
    );
    assert_eq!(count(&new_event_marker, 'B'), 1, "event handled once");
    assert_eq!(count(&new_control_marker, 'B'), 1, "control handled once");

    shutdown(handle, join).await;
}

// ── 10: web_nonloopback_enabled warning fires at startup ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_nonloopback_warning_fires_at_startup() {
    install_capture_subscriber();
    drain_capture(); // discard any startup noise from prior tests

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");

    // Build config with web_allow_nonloopback = true.
    let base = one_display_config(&marker, "400ms");
    let cfg_str = base.replacen(
        "startup_holdoff = \"0s\"",
        "startup_holdoff = \"0s\"\nweb_allow_nonloopback = true",
        1,
    );
    let cfg_path = write_file(dir.path(), "config.toml", &cfg_str);
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let output = drain_capture();
    assert!(
        output.contains("web_nonloopback_enabled"),
        "expected web_nonloopback_enabled event in captured trace output: {output}"
    );

    shutdown(handle, join).await;
}

// ── 11: web_bind_change_ignored fires on reload without rebinding ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn web_bind_change_ignored_on_reload() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");

    // Initial config: no web_port (default None).
    let initial = one_display_config(&marker, "400ms");
    let cfg_path = write_file(dir.path(), "config.toml", &initial);
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    // Wait for the initial blank so the run loop has settled.
    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur"
    );

    drain_capture(); // discard startup events

    // Reload with a config that has a different web_port.
    let modified = initial.replacen(
        "startup_holdoff = \"0s\"",
        "startup_holdoff = \"0s\"\nweb_port = 9999",
        1,
    );
    fs::write(&cfg_path, &modified).unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    let output = drain_capture();
    assert!(
        output.contains("web_bind_change_ignored"),
        "expected web_bind_change_ignored event in captured trace output: {output}"
    );

    // The daemon must still be alive (no crash/rebind) — drive a wake+blank
    // cycle through the injected-events seam to prove the live engine works.
    let events = handle.events_sender();
    events.send(ev("desk", SensorState::Present)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    events.send(ev("desk", SensorState::Absent)).await.unwrap();
    assert!(
        wait_for(|| count(&marker, 'B') >= 2, Duration::from_secs(3)).await,
        "engine must keep working after web_bind_change_ignored"
    );

    shutdown(handle, join).await;
}

// ── 12: render sink wiring (feature-gated) ────────────────────────────────────

// Linux-only on top of the feature gate: the render backend off-Linux is
// dormant-render's no-op stub (`show` always fails E_RENDER_UNAVAILABLE),
// so these daemon-level tests would pin real-Wayland machinery — layer
// surfaces, input-wake drains, overlay/rollback interplay — against a stub
// that has none of it. They only pass there via timing-fragile stub
// fall-through (PR #78 round 10: rollback_input_wake_routes_through_drain
// flaked "expected Rejected, got Reloaded" on macos-latest after four
// green runs). The stub's own contract is unit-pinned in
// dormant-render/src/stub.rs; macOS overlay work is M2 and gets its own
// tests when a real backend exists.
#[cfg(all(feature = "render", target_os = "linux"))]
mod render_smoke {
    use super::*;
    use dormant_core::fakes::RecordingRenderSink;
    use dormant_core::types::StageKind;
    use std::sync::Arc;

    /// Whether a recording sink has logged at least one `Show(RenderBlack)`.
    fn sink_rendered(sink: &RecordingRenderSink) -> bool {
        sink.log().iter().any(|(_dur, cmd)| {
            matches!(
                cmd,
                dormant_core::fakes::RenderCmd::Show {
                    kind: StageKind::RenderBlack,
                    ..
                }
            )
        })
    }

    /// A render sink paired with the `InputWake` sender its generation was
    /// built with — the unit of identity selection in
    /// `rollback_input_wake_routes_through_drain`.
    type WakePair = (
        RecordingRenderSink,
        tokio::sync::mpsc::UnboundedSender<dormant_core::types::DisplayId>,
    );

    fn render_ladder_config(marker: &Path, grace: &str) -> String {
        format!(
            r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon]
controllers = ["command"]
blank_command = "printf B >> '{m}'"
wake_command = "printf W >> '{m}'"
modes = ["power_off"]
ladder = [
  {{ kind = "power_off", dwell = "200ms" }},
  {{ kind = "render_black" }},
]
output = "DP-1"

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
            m = marker.display(),
            g = grace,
        )
    }

    /// A render sink factory that returns a shared [`RecordingRenderSink`]
    /// so the test can inspect recorded commands after the engine runs.
    #[allow(clippy::type_complexity)]
    fn recording_factory(
        sink: RecordingRenderSink,
    ) -> impl Fn(
        dormant_core::types::DisplayId,
        String,
        Option<&tokio::sync::mpsc::UnboundedSender<dormant_core::types::DisplayId>>,
        Option<&dormant_render::ScreensaverSettings>,
        Option<&dormant_render::ShiftSettings>,
    ) -> Option<Arc<dyn dormant_core::traits::RenderSink>>
    + Send
    + Sync
    + 'static {
        move |_did, _output, _tx, _ss, _shift| Some(Arc::new(sink.clone()))
    }

    /// Assembles a config with a render ladder and verifies the
    /// `RecordingRenderSink` is passed through to the engine.  This test
    /// wires the full `assemble_static` → `spawn_generation` pipeline
    /// with an injected render sink.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assembled_render_sink_reaches_engine() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker");
        let cfg_str = render_ladder_config(&marker, "300ms");
        let cfg_path = write_file(dir.path(), "config.toml", &cfg_str);
        let creds_path = dir.path().join("credentials.toml");

        let fake_sink = RecordingRenderSink::new();
        let builder = recording_factory(fake_sink.clone());

        let script = vec![
            (Duration::from_millis(0), ev("desk", SensorState::Present)),
            (Duration::from_millis(200), ev("desk", SensorState::Absent)),
        ];

        let app = App::build_with_sources(
            cfg_path,
            creds_path,
            Strictness::Strict,
            fake_factory("desk", script),
        )
        .expect("build app")
        .with_notify_sink_builder(noop_factory)
        .with_render_sink_builder(builder)
        .disable_ipc();

        let (handle, join) = app.start().await.expect("start app");

        // The command controller blanks successfully with `printf B >> ...`,
        // then the 200ms dwell expires and the machine escalates to
        // RenderBlack — the fake sink should receive show(RenderBlack).
        let ok = wait_for(
            || {
                let log = fake_sink.log();
                log.iter().any(|(_dur, cmd)| {
                    matches!(
                        cmd,
                        dormant_core::fakes::RenderCmd::Show {
                            kind: StageKind::RenderBlack,
                            ..
                        }
                    )
                })
            },
            Duration::from_secs(4),
        )
        .await;

        // Wake the display — the engine should tear down the render surface.
        let events = handle.events_sender();
        let _ = events.send(ev("desk", SensorState::Present)).await;

        let woken = wait_for(
            || {
                let log = fake_sink.log();
                log.iter().any(|(_dur, cmd)| {
                    matches!(cmd, dormant_core::fakes::RenderCmd::Teardown { .. })
                })
                // Note: no wake marker expected — the panel is physically ON
                // during a render overlay; teardown alone reveals it.
            },
            Duration::from_secs(4),
        )
        .await;

        let full_log = fake_sink.log();
        shutdown(handle, join).await;

        assert!(
            ok,
            "expected show(RenderBlack) in render sink, log: {full_log:?}"
        );
        assert!(
            woken,
            "expected teardown after presence restored, log: {full_log:?}"
        );
    }

    /// After a rejected reload triggers `rebuild_old`, the restored
    /// generation uses fresh render sinks with a live `InputWake` channel
    /// — no orphaned senders.  The engine must blank, render, and tear
    /// down correctly after the rollback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollback_render_sink_wiring_is_live() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker");
        let cfg_str = render_ladder_config(&marker, "300ms");
        let cfg_path = write_file(dir.path(), "config.toml", &cfg_str);
        let creds_path = dir.path().join("credentials.toml");

        let fake_sink = RecordingRenderSink::new();
        let builder = recording_factory(fake_sink.clone());

        let script = vec![
            (Duration::from_millis(0), ev("desk", SensorState::Present)),
            (Duration::from_millis(200), ev("desk", SensorState::Absent)),
        ];

        let app = App::build_with_sources(
            cfg_path.clone(),
            creds_path,
            Strictness::Strict,
            fake_factory("desk", script),
        )
        .expect("build app")
        .with_notify_sink_builder(noop_factory)
        .with_render_sink_builder(builder)
        .disable_ipc();

        let (handle, join) = app.start().await.expect("start app");
        let mut reloads = handle.subscribe_reload();

        // Wait for the first show (blank then dwell then render_black).
        let show_ok = wait_for(
            || {
                let log = fake_sink.log();
                log.iter().any(|(_dur, cmd)| {
                    matches!(
                        cmd,
                        dormant_core::fakes::RenderCmd::Show {
                            kind: StageKind::RenderBlack,
                            ..
                        }
                    )
                })
            },
            Duration::from_secs(4),
        )
        .await;
        assert!(show_ok, "expected show(RenderBlack) before rollback");

        // Trigger a rejected reload: write an invalid config.
        fs::write(&cfg_path, "config_version = 1\nbogus_key = true\n").unwrap();
        assert!(handle.trigger_reload().await);

        let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
            .await
            .expect("reload outcome")
            .expect("reload bus open");
        match outcome {
            ReloadOutcome::Rejected(_) => {}
            ReloadOutcome::Reloaded => panic!("expected Rejected, got Reloaded"),
        }

        // rebuild_old ran — verify pending_reload is set.
        let ctl = handle.control_sender();
        let (tx, rx) = oneshot::channel();
        ctl.send(ControlMsg::Snapshot(tx)).await.unwrap();
        let snap: StateSnapshot = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("snapshot")
            .expect("snapshot reply");
        assert!(
            snap.pending_reload.is_some(),
            "pending_reload must be set after rejected reload"
        );

        // The restored generation must still blank + render + wake.
        // Drive a full cycle through the events seam.
        let events = handle.events_sender();
        let _ = events.send(ev("desk", SensorState::Present)).await;
        let teardown_ok = wait_for(
            || {
                let log = fake_sink.log();
                log.iter().any(|(_dur, cmd)| {
                    matches!(cmd, dormant_core::fakes::RenderCmd::Teardown { .. })
                })
            },
            Duration::from_secs(4),
        )
        .await;

        let full_log = fake_sink.log();
        shutdown(handle, join).await;

        assert!(
            teardown_ok,
            "expected teardown after presence in rebuilt generation, log: {full_log:?}"
        );
    }

    /// After a rejected reload triggered by a removed-display wake failure,
    /// the restored generation gets a fresh `InputWake` channel via
    /// `build_render_sinks` → `rebuild_old`.  Sending a `DisplayId` through
    /// that rollback generation's OWN sender must travel the REAL path —
    /// unbounded channel → the drain task spawned by `spawn_generation` →
    /// `ControlMsg::InputWake` → engine → render teardown — the exact wiring
    /// the orphaned-sender bug broke.
    ///
    /// Uses a two-display config where mon2 has `wake_command = "false"`
    /// so its verified wake fails on removal, triggering `rebuild_old`.
    ///
    /// The exact rejected receipt and its matching completion observation make
    /// the restored generation observable without timing a quiet reload bus.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(
        clippy::await_holding_lock,
        reason = "capture_count_lock() serializes this test's reload() call against the exact- \
                  count step-boundary tests it would otherwise contaminate — test-local, always \
                  released promptly at test end"
    )]
    async fn rollback_input_wake_routes_through_drain() {
        let _capture_guard = capture_count_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = TempDir::new().unwrap();
        let m1 = dir.path().join("mon1");
        let m2 = dir.path().join("mon2");

        // Two-display render config.  mon2 has a failing wake command
        // ("false") so its verified wake fails on removal, triggering
        // rebuild_old.
        let cfg_str = format!(
            r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon1]
controllers = ["command"]
blank_command = "printf B >> '{m1}'"
wake_command = "printf W >> '{m1}'"
modes = ["power_off"]
ladder = [
  {{ kind = "power_off", dwell = "200ms" }},
  {{ kind = "render_black" }},
]
output = "DP-1"

[displays.mon2]
controllers = ["command"]
blank_command = "printf B >> '{m2}'"
wake_command = "false"
modes = ["power_off"]
ladder = [
  {{ kind = "power_off", dwell = "20s" }},
  {{ kind = "render_black" }},
]
output = "DP-2"

[rules.r]
zone = "office"
displays = ["mon1", "mon2"]
grace_period = "300ms"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
            m1 = m1.display(),
            m2 = m2.display(),
        );
        let cfg_path = write_file(dir.path(), "config.toml", &cfg_str);
        let creds_path = dir.path().join("credentials.toml");

        // The factory builds a FRESH recording sink per invocation and pairs
        // it with the `InputWake` sender it was handed.  Fresh (not shared)
        // sinks give each generation its own render log, so identity
        // selection can tell rollback sinks apart from initial ones.
        let pairs: Arc<Mutex<Vec<WakePair>>> = Arc::new(Mutex::new(Vec::new()));
        let pairs_for_factory = pairs.clone();
        let builder = move |_did: dormant_core::types::DisplayId,
                            _output: String,
                            tx: Option<
            &tokio::sync::mpsc::UnboundedSender<dormant_core::types::DisplayId>,
        >,
                            _ss: Option<&dormant_render::ScreensaverSettings>,
                            _shift: Option<&dormant_render::ShiftSettings>| {
            let sink = RecordingRenderSink::new();
            if let Some(tx) = tx {
                pairs_for_factory
                    .lock()
                    .unwrap()
                    .push((sink.clone(), tx.clone()));
            }
            Some(Arc::new(sink) as Arc<dyn dormant_core::traits::RenderSink>)
        };

        let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
        let app = App::build_with_sources(
            cfg_path.clone(),
            creds_path,
            Strictness::Strict,
            fake_factory("desk", script),
        )
        .expect("build app")
        .with_notify_sink_builder(noop_factory)
        .with_render_sink_builder(builder)
        .disable_ipc();

        let (handle, join) = app.start().await.expect("start app");
        let mut observations = handle.subscribe_observations();

        // Both displays blank + render — some initial sink shows RenderBlack.
        let show_ok = wait_for(
            || {
                pairs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(sink, _)| sink_rendered(sink))
            },
            Duration::from_secs(6),
        )
        .await;
        assert!(show_ok, "expected show(RenderBlack) before rollback");

        // Reload: mon2 removed → verified wake fails → rebuild_old.
        let cfg_single = format!(
            r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon1]
controllers = ["command"]
blank_command = "printf B >> '{m1}'"
wake_command = "printf W >> '{m1}'"
modes = ["power_off"]
ladder = [
  {{ kind = "power_off", dwell = "200ms" }},
  {{ kind = "render_black" }},
]
output = "DP-1"

[rules.r]
zone = "office"
displays = ["mon1"]
grace_period = "300ms"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
            m1 = m1.display(),
        );
        fs::write(&cfg_path, &cfg_single).unwrap();
        let (request_id, receipt) = handle
            .request_reload_with_id(ReloadSource::Control)
            .await
            .expect("rejected reload receipt");
        assert!(receipt.request_ids.contains(&request_id));
        match &receipt.outcome {
            ReloadOutcome::Rejected(detail) => {
                assert!(
                    detail.contains("mon2"),
                    "reject detail must name un-wakeable display: {detail}"
                );
            }
            ReloadOutcome::Reloaded => {
                panic!("expected Rejected on mon2 wake failure, got Reloaded")
            }
        }
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match observations.recv().await.expect("observation bus open") {
                    dormant_core::observation::DaemonObservation::ReloadCompleted(observed)
                        if observed.request_ids.contains(&request_id) =>
                    {
                        break observed;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("matching reload completion observation");
        assert_eq!(
            observed, receipt,
            "completion observes the restored generation"
        );

        let live_pair = |sinks: &[WakePair]| -> Option<WakePair> {
            let mut rendered = sinks
                .iter()
                .filter(|(sink, tx)| !tx.is_closed() && sink_rendered(sink));
            let pair = rendered.next()?.clone();
            assert!(
                rendered.next().is_none(),
                "one restored render sink is active"
            );
            Some(pair)
        };
        let selected = wait_for(
            || live_pair(&pairs.lock().unwrap()[..]).is_some(),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            selected,
            "no live restored render sink ever rendered — rebuild_old did not \
             rebuild render sinks with a live InputWake channel"
        );
        let (live_sink, live_sender) = live_pair(&pairs.lock().unwrap()[..])
            .expect("a live rollback render sink must exist after the wait");

        // The honest send: push a DisplayId through the SELECTED rollback
        // generation's OWN sender.  It must reach that generation's live
        // drain (unbounded channel → spawn_input_wake_drain →
        // ControlMsg::InputWake → engine).  A SendError here is the
        // orphaned-sender bug — the receiver would be dropped instead of
        // held by a spawned drain.
        live_sender
            .send(dormant_core::types::DisplayId("mon1".into()))
            .expect("rollback generation's InputWake channel must be alive");

        // Teardown must land on THAT SAME sink — proving the wake routed
        // through the selected generation's real channel + drain.
        let teardown_ok = wait_for(
            || {
                live_sink.log().iter().any(|(_dur, cmd)| {
                    matches!(cmd, dormant_core::fakes::RenderCmd::Teardown { .. })
                })
            },
            Duration::from_secs(10),
        )
        .await;

        let final_log = live_sink.log();
        shutdown(handle, join).await;

        assert!(
            teardown_ok,
            "expected teardown on the rollback sink after InputWake, log: {final_log:?}"
        );
    }
}

// ── 13: manual-only (rule-less) display boots and is present in snapshot ─────────

/// Config with a rule-less display "manual" (in [displays] but no rule),
/// plus a rule-referenced display "mon" so the daemon has something to
/// drive.  The test asserts the manual display appears in the snapshot
/// with phase "active" (before the change it was skipped as inert).
fn manual_only_config(mon_marker: &Path, manual_marker: &Path) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

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
blank_command = "printf B >> '{m1}'"
wake_command = "printf W >> '{m1}'"
modes = ["power_off"]

[displays.manual]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m2}'"
wake_command = "printf W >> '{m2}'"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "300ms"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m1 = mon_marker.display(),
        m2 = manual_marker.display(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_only_display_present_in_snapshot() {
    let dir = TempDir::new().unwrap();
    let mon_marker = dir.path().join("mon");
    let manual_marker = dir.path().join("manual");
    let cfg = manual_only_config(&mon_marker, &manual_marker);
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    // Snapshot via control channel.
    let ctl = handle.control_sender();
    let (tx, rx) = oneshot::channel();
    ctl.send(ControlMsg::Snapshot(tx)).await.unwrap();
    let snap: StateSnapshot = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("snapshot in time")
        .expect("snapshot reply");

    // Find the manual display.
    let manual = snap
        .displays
        .iter()
        .find(|(id, _)| id == "manual")
        .map(|(_, ds)| ds);

    shutdown(handle, join).await;

    assert!(
        manual.is_some(),
        "rule-less display 'manual' must be present in snapshot"
    );
    let manual = manual.unwrap();
    assert_eq!(
        manual.phase, "active",
        "rule-less display must start in active phase, got {:?}",
        manual.phase
    );
}

// ── 14: manual-only display not defensive-woken on reload ───────────────────────

/// Config with a rule-driven display "mon" and a manual-only display "manual"
/// (in [displays] but not referenced by any rule).  Both use `command`
/// controllers with separate marker files.
fn manual_and_ruled_config(mon_marker: &Path, manual_marker: &Path, grace: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

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
blank_command = "printf B >> '{m1}'"
wake_command = "printf W >> '{m1}'"
modes = ["power_off"]

[displays.manual]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m2}'"
wake_command = "printf W >> '{m2}'"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m1 = mon_marker.display(),
        m2 = manual_marker.display(),
        g = grace,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn manual_only_display_no_defensive_wake_on_reload() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let mon_marker = dir.path().join("mon");
    let manual_marker = dir.path().join("manual");
    let cfg = manual_and_ruled_config(&mon_marker, &manual_marker, "300ms");
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    // Absent shortly after start → the rule-driven "mon" blanks.  The manual
    // display never blanks on its own (no rule drives it), but we can
    // ForceBlank it through the control channel.
    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    // Wait for the rule-driven display to blank.
    assert!(
        wait_for(|| count(&mon_marker, 'B') >= 1, Duration::from_secs(3)).await,
        "rule-driven display should blank, mon={:?}",
        read(&mon_marker)
    );

    // ForceBlank the manual-only display so it becomes "blanked".
    let ctl = handle.control_sender();
    ctl.send(ControlMsg::ForceBlank(DisplayId("manual".into())))
        .await
        .unwrap();
    // Wait for the blank marker to appear.
    assert!(
        wait_for(|| count(&manual_marker, 'B') >= 1, Duration::from_secs(3)).await,
        "manual display should blank after ForceBlank, manual={:?}",
        read(&manual_marker)
    );
    // Ensure the phase has actually landed at "blanked" before triggering
    // the reload below — the marker byte (waited for above) is written by
    // the command executor before the engine finishes processing the blank
    // RESULT (same hazard as `manual_only_display_full_lifecycle_across_reload`,
    // issue #94 sweep). Without this, the reload could race a still-
    // "blanking" manual display and the post-reload "preserved" assertion
    // would only be preserving the wrong phase.
    assert!(
        wait_for_phase(&ctl, "manual", "blanked", Duration::from_secs(3)).await,
        "manual display's phase must settle to blanked before reload"
    );

    // Clear any W that may have been written (defensive wake at initial
    // holdoff expiry, etc.) and track the exact counts before reload.
    let mon_wake_before = count(&mon_marker, 'W');
    let manual_wake_before = count(&manual_marker, 'W');
    let manual_blank_before = count(&manual_marker, 'B');

    // Reload: rewrite the same config with a different grace so the reload
    // actually takes effect (identical config is a no-op).
    fs::write(
        &cfg_path,
        manual_and_ruled_config(&mon_marker, &manual_marker, "150ms"),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    // Give the defensive wake a moment to fire.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check the manual display: NO additional wake (defensive wake must NOT
    // fire for the rule-less display).
    let manual_content = read(&manual_marker);
    let manual_w_after = count(&manual_marker, 'W');
    assert_eq!(
        manual_w_after, manual_wake_before,
        "manual-only display must NOT be defensive-woken on reload, \
         but got {manual_w_after} W (was {manual_wake_before} W before), content={manual_content:?}",
    );

    // The manual display's phase should still be "blanked" in the snapshot.
    // Retry across the reload generation-switch window (issue #9).
    let snap = snapshot_with_retry(&handle.control_sender()).await;
    let manual_snap = snap
        .displays
        .iter()
        .find(|(id, _)| id == "manual")
        .map(|(_, ds)| ds);
    assert!(
        manual_snap.is_some(),
        "manual display must still be present in snapshot after reload"
    );
    assert_eq!(
        manual_snap.unwrap().phase,
        "blanked",
        "manual-only display must preserve blanked phase after reload, \
         got {:?}",
        manual_snap.unwrap().phase,
    );

    // Rule-driven "mon" SHOULD be defensive-woken (existing behavior).
    let mon_wake_after = count(&mon_marker, 'W');
    assert!(
        mon_wake_after > mon_wake_before,
        "rule-driven dark display must still be defensive-woken on reload, \
         mon_wake_before={mon_wake_before} mon_wake_after={mon_wake_after} mon={:?}",
        read(&mon_marker)
    );

    // Manual should NOT have lost its blank (no extra B either).
    assert_eq!(
        count(&manual_marker, 'B'),
        manual_blank_before,
        "manual-only blanked display must NOT re-blank after reload"
    );

    shutdown(handle, join).await;
}

// ── 15: rule-driven dark display defensive-woken on reload (regression) ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn rule_driven_dark_display_defensive_woken_on_reload() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "150ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "display should blank before reload"
    );
    let w_before = count(&marker, 'W');

    // Reload: different grace forces a real reload cycle.
    fs::write(&cfg_path, one_display_config(&marker, "300ms")).unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    assert!(
        wait_for(|| count(&marker, 'W') > w_before, Duration::from_secs(3)).await,
        "rule-driven dark display must receive a defensive wake on reload, \
         w_before={w_before} marker={:?}",
        read(&marker)
    );

    shutdown(handle, join).await;
}

// ── 16: manual-only display full lifecycle across reload ─────────────────────────

/// A manual-only display survives a full `ForceBlank` → reload → `ForceWake`
/// cycle.  Composes T1 (built), T4 (phase preserved across reload), and
/// manual control (`ForceBlank` / `ForceWake`).
///
/// Drives a single watcher reload (the real production path — `fs::write`
/// triggers the config-file watcher).  Do NOT also call `trigger_reload()`:
/// a concurrent double reload races the generation swap and can lose a
/// manual command (tracked in issue #9).
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_only_display_full_lifecycle_across_reload() {
    let dir = TempDir::new().unwrap();
    let mon_marker = dir.path().join("mon");
    let manual_marker = dir.path().join("manual");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &manual_and_ruled_config(&mon_marker, &manual_marker, "300ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // No sensor events — the manual display starts active (no rule drives
    // it).  The rule-driven "mon" will blank after its 300ms grace, but
    // that is irrelevant to the manual-only lifecycle under test.
    let observations = ObservationHub::new(64);
    let mut observation_rx = observations.subscribe();
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", vec![]),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_observation_hub(observations)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let ctl = handle.control_sender();

    // ── 1. ForceBlank the manual-only display ──────────────────────────
    ctl.send(ControlMsg::ForceBlank(DisplayId("manual".into())))
        .await
        .unwrap();
    assert!(
        wait_for(|| count(&manual_marker, 'B') >= 1, Duration::from_secs(3)).await,
        "manual display must blank after ForceBlank, marker={:?}",
        read(&manual_marker)
    );
    let blanking = recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "manual active to blanking",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(0),
                    rule_id: None,
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Active,
                    new_phase: dormant_core::state_machine::Phase::Blanking,
                } if display_id.0 == "manual"
            )
        },
    )
    .await;
    assert!(matches!(
        blanking,
        DaemonObservation::DisplayPhaseChanged { .. }
    ));
    let blanked = recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "manual blanking to blanked",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(0),
                    rule_id: None,
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Blanking,
                    new_phase: dormant_core::state_machine::Phase::Blanked,
                } if display_id.0 == "manual"
            )
        },
    )
    .await;
    assert!(matches!(
        blanked,
        DaemonObservation::DisplayPhaseChanged { .. }
    ));

    let manual_wake_before = count(&manual_marker, 'W');

    // ── 3. Reload via the config-file watcher ──────────────────────────
    // Changing the grace triggers a real reload (not a no-op).  Only
    // `fs::write` — the watcher arm fires the reload.  Calling
    // `trigger_reload()` as well would create two concurrent reloads that
    // race the generation swap (issue #9).
    fs::write(
        &cfg_path,
        manual_and_ruled_config(&mon_marker, &manual_marker, "150ms"),
    )
    .unwrap();
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation zero drained",
        |observation| {
            matches!(
                observation,
                DaemonObservation::GenerationDrained {
                    generation: GenerationId(0)
                }
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "manual phase restored into generation one",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(1),
                    rule_id: None,
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Active,
                    new_phase: dormant_core::state_machine::Phase::Blanked,
                } if display_id.0 == "manual"
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation one started",
        |observation| {
            matches!(
                observation,
                DaemonObservation::GenerationStarted {
                    generation: GenerationId(1)
                }
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "reload completed for generation one",
        |observation| {
            matches!(
                observation,
                DaemonObservation::ReloadCompleted(receipt)
                    if receipt.generation == GenerationId(1)
                        && receipt.outcome == ReloadOutcome::Reloaded
            )
        },
    )
    .await;

    // ── 4. Assert across reload: NO wake, phase preserved ──────────────
    let manual_w_after = count(&manual_marker, 'W');
    assert_eq!(
        manual_w_after,
        manual_wake_before,
        "manual-only display must NOT be defensive-woken on reload, \
         got {manual_w_after} W (was {manual_wake_before} W before), marker={:?}",
        read(&manual_marker)
    );

    // ── 5. ForceWake the manual-only display ───────────────────────────
    ctl.send(ControlMsg::ForceWake(DisplayId("manual".into())))
        .await
        .unwrap();
    assert!(
        wait_for(
            || count(&manual_marker, 'W') > manual_wake_before,
            Duration::from_secs(3)
        )
        .await,
        "manual display must wake after ForceWake, marker={:?}",
        read(&manual_marker)
    );
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "manual blanked to waking",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(1),
                    rule_id: None,
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Blanked,
                    new_phase: dormant_core::state_machine::Phase::Waking,
                } if display_id.0 == "manual"
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "manual waking to active",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(1),
                    rule_id: None,
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Waking,
                    new_phase: dormant_core::state_machine::Phase::Active,
                } if display_id.0 == "manual"
            )
        },
    )
    .await;

    shutdown(handle, join).await;
}

// ── Wear tracker (T7) ────────────────────────────────────────────────────────

/// `XDG_STATE_HOME` is process-global env; these tests are the only ones in
/// this binary that touch it, so a dedicated mutex held for the lifetime of
/// each test (set → run app → read files → restore) is sufficient to keep
/// them from racing each other under `cargo test`'s default multi-threaded
/// runner. Mirrors the pattern in `dormant-tray/src/icon.rs`'s
/// `load_does_not_depend_on_out_dir_at_runtime` test.
static WEAR_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn wear_env_lock() -> &'static Mutex<()> {
    WEAR_ENV_LOCK.get_or_init(|| Mutex::new(()))
}

/// Point `XDG_STATE_HOME` at `dir` for the duration of the guard, restoring
/// the previous value (or unsetting) on drop.
struct XdgStateHomeGuard {
    prev: Option<std::ffi::OsString>,
}

impl XdgStateHomeGuard {
    fn set(dir: &Path) -> Self {
        let prev = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: caller holds `wear_env_lock()` for the guard's lifetime,
        // so no other thread in this process observes env::* concurrently.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", dir);
        }
        Self { prev }
    }
}

impl Drop for XdgStateHomeGuard {
    fn drop(&mut self) {
        // SAFETY: see `set` above.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }
}

/// [`one_display_config`] plus a `[wear]` section (appended TOML — later
/// keys win nothing here since `[wear]` only appears once).
fn one_display_config_with_wear(marker: &Path, grace: &str, wear_toml: &str) -> String {
    format!("{}\n{wear_toml}", one_display_config(marker, grace))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes the 3 wear tests' shared XDG_STATE_HOME env var across \
              the whole test body (app lifetime included) — the lock is test-local (only these \
              3 tests touch it) and always released promptly at test end, so holding it across \
              awaits here cannot deadlock or starve unrelated tests"
)]
async fn wear_ledger_file_appears_and_seeds() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg = one_display_config_with_wear(
        &marker,
        "50ms",
        "[wear]\nenabled = true\nsample_interval = \"5s\"\npersist_interval = \"5s\"\nread_timeout = \"500ms\"\n",
    );
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    // `tokio::time::interval`'s FIRST tick fires immediately (not after a
    // full period), so the first sample/persist happens right away despite
    // the 5s-floor `sample_interval` validation requires.
    let wear_file = state_home
        .path()
        .join("dormant")
        .join("wear")
        .join("wear-mon.json");
    let ok = wait_for(|| wear_file.exists(), Duration::from_secs(3)).await;

    let ledger_check = ok.then(|| {
        let contents = read(&wear_file);
        serde_json::from_str::<dormant_core::wear::WearLedger>(&contents)
    });

    shutdown(handle, join).await;

    assert!(
        ok,
        "wear ledger file for 'mon' must appear under XDG_STATE_HOME/dormant/wear"
    );
    let ledger = ledger_check.unwrap().expect("ledger file must parse");
    assert_eq!(
        ledger.seeded_usage_hours, None,
        "command controller has no usage-hours readback — seed must stay None"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes the 3 wear tests' shared XDG_STATE_HOME env var across \
              the whole test body (app lifetime included) — the lock is test-local (only these \
              3 tests touch it) and always released promptly at test end, so holding it across \
              awaits here cannot deadlock or starve unrelated tests"
)]
async fn wear_disabled_creates_nothing() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // #47 fix (§1 — test determinism): this test used to prove absence with
    // a flat `sleep(300ms)` then a directory read. That 300ms had NO
    // synchronization with the tracker's own lifecycle — it was purely a
    // best-effort margin against the OLD bug (an unrelated test's tracker,
    // spawned with `wear.enabled` defaulting `true`, reading
    // `wear_state_dir()` — i.e. `XDG_STATE_HOME` — from inside its own
    // detached task and landing inside THIS test's `wear_env_lock`-protected
    // env mutation window; see `WearTrackerDeps::dir`'s doc comment). Now
    // that the state directory is resolved synchronously in `App::start`
    // and threaded through as a plain `PathBuf` — no component under test
    // ever reads `XDG_STATE_HOME` again — the sleep's only remaining job is
    // "give the disabled tracker's own first tick time to park", which IS
    // observable: it logs `event = "wear_tracker_parked"` from the very
    // tick this test cares about. Block on that event instead of a fixed
    // wall-clock guess, and name the actual owner (tracker parked or not,
    // which path leaked) in the failure message rather than a bare
    // left/right mismatch.
    install_capture_subscriber();
    drain_capture(); // discard startup/prior-test noise

    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg = one_display_config_with_wear(
        &marker,
        "50ms",
        "[wear]\nenabled = false\nsample_interval = \"5s\"\npersist_interval = \"5s\"\n",
    );
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    // Block on the tracker's own `wear_tracker_parked` event (its first
    // tick fires immediately regardless of `sample_interval`, so this
    // resolves almost instantly in the common case) instead of guessing a
    // fixed sleep duration. Only `wear_disabled_creates_nothing` and
    // `wear_park_persists_final_ledger` can ever emit this event, and
    // `wear_env_lock` (held for this test's whole body) already excludes
    // the latter from running concurrently — so this substring check
    // cannot be contaminated by a sibling wear test.
    let mut captured = String::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let parked = loop {
        captured.push_str(&drain_capture());
        if captured.contains("wear_tracker_parked") {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let wear_dir = state_home.path().join("dormant").join("wear");
    let entries: Vec<PathBuf> = fs::read_dir(&wear_dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();

    shutdown(handle, join).await;

    assert!(
        parked,
        "disabled tracker never logged wear_tracker_parked within 2s — cannot prove the \
         directory check below observed a settled tracker; captured trace:\n{captured}"
    );
    assert!(
        entries.is_empty(),
        "wear.enabled = false must create no files under the wear state dir; found {entries:?} \
         (tracker reported parked={parked})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes the 3 wear tests' shared XDG_STATE_HOME env var across \
              the whole test body (app lifetime included) — the lock is test-local (only these \
              3 tests touch it) and always released promptly at test end, so holding it across \
              awaits here cannot deadlock or starve unrelated tests"
)]
async fn wear_survives_reload_and_fail_closes_during_swap() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // This test drives 3 back-to-back reloads (below) — each is a real
    // Runner::reload() call that emits the same step-boundary markers the
    // exact-count watchdog tests count, so it also serializes on
    // capture_count_lock (see that lock's doc comment).
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    // sample_interval sits at the 5s validation floor (E_CONFIG_INVALID
    // below that) — `tokio::time::interval`'s first tick fires immediately,
    // so the initial ledger still appears right away; a SECOND tick (proving
    // continued accrual after the reload churn) needs a ~5s real-time wait.
    let cfg = one_display_config_with_wear(
        &marker,
        "50ms",
        "[wear]\nenabled = true\nsample_interval = \"5s\"\npersist_interval = \"5s\"\nread_timeout = \"500ms\"\n",
    );
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let wear_file = state_home
        .path()
        .join("dormant")
        .join("wear")
        .join("wear-mon.json");
    assert!(
        wait_for(|| wear_file.exists(), Duration::from_secs(3)).await,
        "ledger must appear before the reload churn starts"
    );
    let sample_count_before =
        serde_json::from_str::<dormant_core::wear::WearLedger>(&read(&wear_file))
            .expect("ledger file must parse")
            .sample_count;

    // Trigger several reloads back-to-back — each one empties the executor
    // watch immediately before teardown and republishes it once the new
    // generation installs. The tracker must never panic against a dead
    // executor mid-swap.
    for _ in 0..3 {
        assert!(
            handle.trigger_reload().await,
            "reload trigger must be accepted"
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    // Cross the 5s sample_interval boundary so a second tick fires
    // post-reload, proving the tracker resumed (not just survived).
    let ok = wait_for(
        || {
            serde_json::from_str::<dormant_core::wear::WearLedger>(&read(&wear_file))
                .is_ok_and(|l| l.sample_count > sample_count_before)
        },
        Duration::from_secs(9),
    )
    .await;

    let contents = read(&wear_file);
    let ledger: dormant_core::wear::WearLedger =
        serde_json::from_str(&contents).expect("ledger file must still parse after reload churn");

    // The run-loop task must still be alive (no panic unwound it) —
    // `shutdown` bounds the wait and would time out on a hung/panicked task.
    shutdown(handle, join).await;

    assert!(
        ok && ledger.sample_count > sample_count_before,
        "tracker must keep sampling/persisting across reload churn without panicking \
         (before={sample_count_before}, after={})",
        ledger.sample_count
    );
}

/// T7 review M3 (RED-first without the fix): spec §2/§4.2 require a
/// "final persist" on daemon shutdown, in addition to the periodic
/// `persist_interval` cadence. `persist_interval` is set far longer than
/// `sample_interval` here so a sample-only tick accumulates in memory
/// between periodic persists; shutting down mid-window must flush that
/// accumulated wear to disk, not leave the file stuck at whatever
/// `sample_count` the last periodic persist wrote.
///
/// **Immediate read, not a post-shutdown poll (#47 fix, plan Task 5 §4)**:
/// this test used to poll (bounded, up to 8s) after `shutdown()` because
/// `App::start`'s wear-tracker `JoinHandle` was fire-and-forget (dropped,
/// never joined) — `shutdown()` only awaited the daemon's main `run_loop`
/// task, which could return before the SEPARATE wear-tracker task finished
/// its own cancellation-triggered `persist_all_dirty`, so reading the file
/// immediately after `shutdown()` used to race that fire-and-forget
/// completion (confirmed via a temporary `tracing`-capture diagnostic run:
/// the in-memory ledger's `sample_count` climbed correctly tick-by-tick,
/// but the file still showed the stale pre-shutdown value on the losing
/// side of the race). `run_loop` now retains the tracker's `JoinHandle` and
/// bounded-awaits it (mirroring the engine's own bounded-join-then-abort
/// teardown) before returning, so by the time `shutdown()`'s `join.await`
/// resolves, the tracker's final persist has already either landed or been
/// force-aborted after its own 5s grace window — there is no longer a
/// window to poll for. A direct read replaces the old workaround.
///
/// **G1 review fix (margin widened)**: the pre-shutdown wait used to be a
/// fixed `sleep(7s)` against a 5s `sample_interval` — only a ~2s margin
/// for "at least one more tick landed in memory" to hold. There is no
/// observable signal to `wait_for`-poll on for that in-memory accumulation
/// (the file isn't touched again until either the 60s `persist_interval`
/// boundary or the shutdown flush itself, so polling the file here would
/// just re-detect the same first-persist write). The only lever available
/// is wall-clock margin, so it's widened to comfortably cover three tick
/// periods plus flat slack (`3 * sample_interval + 5s` = 20s, up from 7s)
/// — enough that a single slipped tick under CPU/scheduler contention
/// still leaves at least one more tick landed before shutdown fires,
/// without weakening the assertion itself (still `sample_count >` the
/// first-persist count).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes the wear tests' shared XDG_STATE_HOME env var across \
              the whole test body (app lifetime included) — the lock is test-local and always \
              released promptly at test end, so holding it across awaits here cannot deadlock \
              or starve unrelated tests"
)]
async fn wear_shutdown_persists_final_ledger() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    // persist_interval is deliberately far longer than sample_interval so
    // the periodic cadence alone will NOT persist again during this test's
    // window — only the first (immediate) tick and the final shutdown
    // flush should ever touch the file.
    let sample_interval = Duration::from_secs(5);
    let cfg = one_display_config_with_wear(
        &marker,
        "50ms",
        &format!(
            "[wear]\nenabled = true\nsample_interval = \"{}s\"\npersist_interval = \"60s\"\nread_timeout = \"500ms\"\n",
            sample_interval.as_secs()
        ),
    );
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let wear_file = state_home
        .path()
        .join("dormant")
        .join("wear")
        .join("wear-mon.json");
    assert!(
        wait_for(|| wear_file.exists(), Duration::from_secs(3)).await,
        "ledger must appear from the first (immediate) tick's persist"
    );
    let sample_count_at_first_persist =
        serde_json::from_str::<dormant_core::wear::WearLedger>(&read(&wear_file))
            .expect("ledger file must parse")
            .sample_count;

    // Let at least one more sample_interval tick accumulate wear IN MEMORY
    // without crossing the 60s persist_interval boundary again. See the
    // G1 review fix note in this test's doc comment for why this can't be
    // a `wait_for` poll (no observable signal before the flush) and why
    // the margin is now `3 * sample_interval + 5s` slack instead of a
    // fixed 7s.
    tokio::time::sleep(sample_interval * 3 + Duration::from_secs(5)).await;

    // The run-loop join is the final-persist completion signal: it only
    // resolves after its bounded wear-tracker join has completed or aborted.
    // Unlike the general smoke-test helper, this assertion must not hide a
    // timeout and read the ledger while the tracker is still writing.
    handle.shutdown();
    tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("daemon must complete the final wear persist")
        .expect("daemon run loop must not panic");

    let contents = read(&wear_file);
    let ledger: dormant_core::wear::WearLedger =
        serde_json::from_str(&contents).expect("ledger file must still parse after shutdown");

    assert!(
        ledger.sample_count > sample_count_at_first_persist,
        "shutdown must flush wear accumulated since the last periodic persist \
         (at first persist={sample_count_at_first_persist}, after shutdown={})",
        ledger.sample_count
    );
}

/// T7 review M3 (RED-first without the fix): spec §2 — "Flipping false
/// parks it after a final persist." Mirrors
/// `wear_shutdown_persists_final_ledger` but the trigger is a runtime
/// `[wear] enabled = false` reload instead of daemon shutdown. Unlike that
/// test, this one already polls (`wait_for`) rather than reading
/// immediately after the trigger, so it isn't sensitive to the wear
/// tracker's fire-and-forget task completion timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes the wear tests' shared XDG_STATE_HOME env var across \
              the whole test body (app lifetime included) — the lock is test-local and always \
              released promptly at test end, so holding it across awaits here cannot deadlock \
              or starve unrelated tests"
)]
async fn wear_park_persists_final_ledger() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The [wear].enabled = false reload below is a real Runner::reload()
    // call that emits the same step-boundary markers the exact-count
    // watchdog tests count, so it also serializes on capture_count_lock
    // (see that lock's doc comment).
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = dir.path().join("config.toml");
    let enabled_cfg = one_display_config_with_wear(
        &marker,
        "50ms",
        "[wear]\nenabled = true\nsample_interval = \"5s\"\npersist_interval = \"60s\"\nread_timeout = \"500ms\"\n",
    );
    write_file(dir.path(), "config.toml", &enabled_cfg);
    let creds_path = dir.path().join("credentials.toml");

    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let wear_file = state_home
        .path()
        .join("dormant")
        .join("wear")
        .join("wear-mon.json");
    assert!(
        wait_for(|| wear_file.exists(), Duration::from_secs(3)).await,
        "ledger must appear from the first (immediate) tick's persist"
    );
    let sample_count_at_first_persist =
        serde_json::from_str::<dormant_core::wear::WearLedger>(&read(&wear_file))
            .expect("ledger file must parse")
            .sample_count;

    // Let at least one more sample_interval tick accumulate wear IN MEMORY
    // (persist_interval is 60s, so no periodic persist fires again yet).
    tokio::time::sleep(Duration::from_secs(7)).await;

    // Flip [wear].enabled = false on disk and trigger a reload — the
    // tracker's next tick must detect the park edge and flush before
    // continuing (T7 review M3), per spec §2.
    let disabled_cfg = one_display_config_with_wear(
        &marker,
        "50ms",
        "[wear]\nenabled = false\nsample_interval = \"5s\"\npersist_interval = \"60s\"\nread_timeout = \"500ms\"\n",
    );
    write_file(dir.path(), "config.toml", &disabled_cfg);
    assert!(
        handle.trigger_reload().await,
        "reload to wear.enabled=false must be accepted"
    );

    let ok = wait_for(
        || {
            serde_json::from_str::<dormant_core::wear::WearLedger>(&read(&wear_file))
                .is_ok_and(|l| l.sample_count > sample_count_at_first_persist)
        },
        Duration::from_secs(8),
    )
    .await;

    let contents = read(&wear_file);
    let ledger: dormant_core::wear::WearLedger =
        serde_json::from_str(&contents).expect("ledger file must still parse after park");

    shutdown(handle, join).await;

    assert!(
        ok && ledger.sample_count > sample_count_at_first_persist,
        "the park edge must flush wear accumulated since the last periodic persist \
         (at first persist={sample_count_at_first_persist}, after park={})",
        ledger.sample_count
    );
}

// ── 17: wake-failure-surfacing over real IPC (T3/P8, un-reloaded path) ─────────
//
// Exercises `blank_failure` / `blank_recovered` event delivery and the
// `last_blank_failed` snapshot flag over the ACTUAL Unix socket via
// `dormantctl::client` (never `.disable_ipc()`). The `command` controller's
// outcome is flipped by creating a flag file that its shell command tests
// for — the config's `blank_command` STRING itself never changes and no
// reload happens here, so this test is orthogonal to the dispatch-relevant
// voiding gate exercised by test 18 below.

/// One ruled `command` display whose blank succeeds iff `flag` exists
/// (`test -e`), real IPC via `socket_path`.
fn ipc_failure_config(socket_path: &Path, flag: &Path, grace: &str) -> String {
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
blank_command = "test -e '{flag}'"
wake_command = "/bin/true"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        sock = socket_path.display(),
        flag = flag.display(),
        g = grace,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn ipc_event_stream_surfaces_blank_failure_then_recovery() {
    let dir = TempDir::new().unwrap();
    // A separate tempdir for the socket — never the daemon's default path.
    let sock_dir = TempDir::new().unwrap();
    let socket_path = sock_dir.path().join("dormant.sock");
    let flag = dir.path().join("blank_ok_flag");

    let cfg = ipc_failure_config(&socket_path, &flag, "50ms");
    let cfg_path = write_file(dir.path(), "config.toml", &cfg);
    let creds_path = dir.path().join("credentials.toml");

    // A single vacancy edge is enough: a failed blank re-enters Grace
    // directly (state_machine.rs "blank_failed_regrace") and keeps retrying
    // on its own every `grace_period` while the zone stays absent — no
    // further scripted edges are needed to drive the recovery retry either.
    let script = vec![
        (Duration::from_millis(0), ev("desk", SensorState::Present)),
        (Duration::from_millis(100), ev("desk", SensorState::Absent)),
    ];
    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory);
    // Real IPC — this test needs the actual socket for event subscription.
    let (handle, join) = app.start().await.expect("start app");

    assert!(
        wait_for(|| socket_path.exists(), Duration::from_secs(3)).await,
        "IPC socket must be bound"
    );

    let connect_path = socket_path.clone();
    let (event_stream, event_shutdown) =
        tokio::task::spawn_blocking(move || dormantctl::client::connect_events(&connect_path))
            .await
            .expect("connect_events join")
            .expect("connect to daemon event stream");

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<DaemonEvent>();
    let pump = tokio::task::spawn_blocking(move || {
        for item in event_stream {
            let Ok(ev) = item else { break };
            if event_tx.send(ev).is_err() {
                break;
            }
        }
    });

    // ── Phase 1: blank fails (flag absent) ──────────────────────────────
    let saw_failure = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match event_rx.recv().await {
                Some(DaemonEvent::BlankFailure { display, .. }) => {
                    assert_eq!(display.0, "mon");
                    return;
                }
                Some(_) => {}
                None => panic!("event stream closed before BlankFailure"),
            }
        }
    })
    .await;
    assert!(saw_failure.is_ok(), "expected a BlankFailure event in time");

    let status_path = socket_path.clone();
    let resp = tokio::task::spawn_blocking(move || {
        dormantctl::client::send_request(&status_path, &IpcRequest::Status)
    })
    .await
    .expect("status join")
    .expect("status request");
    let snap = resp.snapshot.expect("Status response carries a snapshot");
    let d = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("mon in snapshot");
    assert!(
        d.1.last_blank_failed,
        "snapshot must show last_blank_failed=true after the failed blank"
    );

    // ── Phase 2: flip the underlying command's outcome (no reload) ─────
    fs::write(&flag, "").expect("create flag file");

    let saw_recovery = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match event_rx.recv().await {
                Some(DaemonEvent::BlankRecovered { display }) => {
                    assert_eq!(display.0, "mon");
                    return;
                }
                Some(_) => {}
                None => panic!("event stream closed before BlankRecovered"),
            }
        }
    })
    .await;
    assert!(
        saw_recovery.is_ok(),
        "expected a BlankRecovered event in time"
    );

    let status_path = socket_path.clone();
    let resp = tokio::task::spawn_blocking(move || {
        dormantctl::client::send_request(&status_path, &IpcRequest::Status)
    })
    .await
    .expect("status join")
    .expect("status request");
    let snap = resp.snapshot.expect("Status response carries a snapshot");
    let d = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("mon in snapshot");
    assert!(
        !d.1.last_blank_failed,
        "snapshot flag must clear after recovery"
    );

    // Unblock the blocking event-pump thread before shutdown tears the
    // socket down from the server side anyway.
    let _ = event_shutdown.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;

    shutdown(handle, join).await;
}

// ── 18: reload carry-over + dispatch-relevant voiding gate (T3) ────────────────
//
// `.disable_ipc()` — driven entirely via `handle.control_sender()` +
// `handle.trigger_reload()`, mirroring the file's existing reload tests.
// Same `command` display shape as test 17 (blank succeeds iff a flag file
// exists), but the flag is NEVER created here — the point is carry-over /
// voiding of `last_blank_failed`, not an actual recovery.
//
// Reload #2's dispatch-relevant edit swaps `blank_command` for a DIFFERENT
// command that still always fails (not a command that starts succeeding) —
// see the comment on reload #2 below for why that de-confounds the "flag
// cleared" assertion from a coincidental real recovery (T3-review Must-1).

/// One ruled `command` display; `log_level` and `blank_command` are the two
/// knobs this test flips independently (unrelated daemon-level edit vs. a
/// dispatch-relevant per-display edit).
fn reload_carry_config(marker: &Path, blank_cmd: &str, log_level: &str, grace: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"
log_level = "{log_level}"

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
blank_command = "{blank_cmd}"
wake_command = "printf W >> '{m}'"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        log_level = log_level,
        blank_cmd = blank_cmd,
        m = marker.display(),
        g = grace,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() calls against the exact- \
              count step-boundary tests they would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn reload_carries_last_blank_failed_until_dispatch_relevant_edit() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let flag = dir.path().join("blank_ok_flag"); // never created in this test
    let blank_cmd = format!("test -e '{}'", flag.display());

    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &reload_carry_config(&marker, &blank_cmd, "info", "50ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    let script = vec![
        (Duration::from_millis(0), ev("desk", SensorState::Present)),
        (Duration::from_millis(50), ev("desk", SensorState::Absent)),
    ];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();
    let ctl = handle.control_sender();

    // ── Drive a blank failure ───────────────────────────────────────────
    assert!(
        wait_for_last_blank_failed(&ctl, "mon", true, Duration::from_secs(3)).await,
        "expected last_blank_failed=true after the scripted vacancy edge"
    );

    // ── Reload #1: unrelated daemon-level edit — same display block ────
    fs::write(
        &cfg_path,
        reload_carry_config(&marker, &blank_cmd, "debug", "50ms"),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload #1 outcome in time")
        .expect("reload bus open");
    assert_eq!(
        outcome,
        ReloadOutcome::Reloaded,
        "reload #1 must be accepted"
    );

    assert!(
        wait_for_last_blank_failed(&ctl, "mon", true, Duration::from_secs(3)).await,
        "unrelated config edit must carry last_blank_failed=true forward"
    );

    // ── Reload #2: dispatch-relevant edit that STAYS FAILING ───────────────
    //
    // `blank_command` changes to a DIFFERENT command ("false" instead of
    // `test -e '<flag>'`) — dispatch-relevant per `dispatch_relevant_eq`
    // (the literal string differs), but the new command never succeeds
    // either. If `last_blank_failed` reads `false` after this reload it can
    // ONLY be explained by the voiding gate proactively zeroing the carried
    // evidence at seed time — a coincidental real recovery is impossible
    // here (unlike a `/bin/true` swap, which would confound the assertion:
    // see T3-review Must-1).
    //
    // `grace_period` is also bumped to a long value for this reload only.
    // The rebuilt (re-converged) state machine restarts `Active`, and with
    // the zone still absent it would otherwise re-attempt a blank after the
    // grace elapses — which would fail again and flip the flag back to
    // `true` for a reason that has nothing to do with the voiding gate.
    // `grace_period` lives on the RULE, not `DisplayConfig`, so it plays no
    // part in `dispatch_relevant_eq`'s classification; it exists purely to
    // pin the assertion window before any new dispatch can occur.
    fs::write(
        &cfg_path,
        reload_carry_config(&marker, "false", "debug", "10s"),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload #2 outcome in time")
        .expect("reload bus open");
    assert_eq!(
        outcome,
        ReloadOutcome::Reloaded,
        "reload #2 must be accepted"
    );

    // Well inside the 10s grace window — no new blank dispatch can have run
    // yet, so this can only be the seed-time voiding gate at work.
    assert!(
        wait_for_last_blank_failed(&ctl, "mon", false, Duration::from_secs(2)).await,
        "dispatch-relevant blank_command edit must void (zero) the carried failure evidence"
    );
    let snap = snapshot_with_retry(&ctl).await;
    let d = snap
        .displays
        .iter()
        .find(|(id, _)| id == "mon")
        .expect("mon in snapshot");
    assert_eq!(
        d.1.wake_attempts, 0,
        "wake_attempts must also be zeroed by the voiding gate"
    );
    assert!(
        !d.1.last_blank_failed,
        "last_blank_failed must still read false within the grace window \
         (ruling out a coincidental re-failure re-setting it)"
    );

    shutdown(handle, join).await;
}

// ── 19: notifier cross-generation close via startup reconcile (T4, R3-S/C) ────
//
// A wake failure past `[notifications].wake_attempt_threshold` must produce
// a `Send` on the injected `RecordingSink`. A subsequent reload with a
// dispatch-relevant `wake_command` edit (still always-failing — never
// `/bin/true`, so a coincidental REAL recovery cannot explain the result,
// mirroring test 18's de-confounding rationale) voids the carried
// `wake_attempts` evidence. The notifier's `NotifyState` is daemon-lifetime
// (shared across generations via `App::start`), so the NEW generation's
// startup `reconcile` sees the (now healthy) snapshot with the OLD open
// episode still recorded and must `Close` it — with no recovery `Send`,
// since this is voided evidence, not a real recovery.

/// A `NotifySink` fake that records every `notify`/`close` call. Shared via
/// `Arc` between the test and the `App`'s injected builder so the SAME
/// instance observes both generations.
#[derive(Default)]
struct RecordingSink {
    notifies: Mutex<Vec<(String, String, u8, u32)>>,
    closes: Mutex<Vec<u32>>,
    next_id: std::sync::atomic::AtomicU32,
}

#[async_trait::async_trait]
impl NotifySink for RecordingSink {
    async fn notify(
        &self,
        summary: &str,
        body: &str,
        urgency: u8,
        replaces: u32,
    ) -> Result<u32, String> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.notifies.lock().unwrap().push((
            summary.to_string(),
            body.to_string(),
            urgency,
            replaces,
        ));
        Ok(id)
    }

    async fn close(&self, id: u32) -> Result<(), String> {
        self.closes.lock().unwrap().push(id);
        Ok(())
    }
}

/// One ruled `command` display whose wake command is the tunable `wake_cmd`
/// (always a failing shell command in this test — only the literal string
/// changes between generations). Fast grace/wake-retry so the threshold is
/// crossed quickly.
fn notifier_reload_config(marker: &Path, wake_cmd: &str, grace: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[notifications]
wake_attempt_threshold = 2
cooldown = "1m"

[displays.mon]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m}'"
wake_command = "{wake_cmd}"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m = marker.display(),
        wake_cmd = wake_cmd,
        g = grace,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() call against the exact- \
              count step-boundary tests it would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn notifier_closes_stale_episode_from_new_generation_startup_reconcile() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");

    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &notifier_reload_config(&marker, "false", "20ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // Absent long enough for the grace period to elapse (blank succeeds via
    // the marker-writing command), then present again to trigger a wake —
    // `wake_command = "false"` always fails, so the state machine retries on
    // `wake_retry_interval` (20ms) indefinitely (the wake-wedge invariant),
    // giving the notifier several `WakeRetry` events past the threshold.
    let script = vec![
        (Duration::from_millis(0), ev("desk", SensorState::Absent)),
        (Duration::from_millis(150), ev("desk", SensorState::Present)),
    ];

    let sink = std::sync::Arc::new(RecordingSink::default());
    let sink_for_builder = sink.clone();
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .disable_ipc()
    .with_notify_sink_builder(move || sink_for_builder.clone() as std::sync::Arc<dyn NotifySink>);
    let (handle, join) = app.start().await.expect("start app");

    // ── Generation 1: drive a wake failure past the threshold ──────────────
    assert!(
        wait_for(
            || !sink.notifies.lock().unwrap().is_empty(),
            Duration::from_secs(5),
        )
        .await,
        "expected the notifier to Send once wake_attempts crossed the threshold"
    );
    let sent_count = sink.notifies.lock().unwrap().len();
    assert_eq!(sent_count, 1, "expected exactly one Send before the reload");
    assert!(
        sink.closes.lock().unwrap().is_empty(),
        "no Close expected yet"
    );

    // ── Reload: dispatch-relevant wake_command edit, STILL always-failing ──
    // (never `/bin/true` — see test 18's comment on why that would confound
    // the assertion with a coincidental real recovery).
    let mut reloads = handle.subscribe_reload();
    fs::write(&cfg_path, notifier_reload_config(&marker, "exit 1", "20ms")).unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded, "reload must be accepted");

    // ── Generation 2's startup reconcile must Close the stale episode ──────
    assert!(
        wait_for(
            || !sink.closes.lock().unwrap().is_empty(),
            Duration::from_secs(5),
        )
        .await,
        "expected the new generation's startup reconcile to Close the voided episode"
    );
    assert_eq!(
        sink.notifies.lock().unwrap().len(),
        1,
        "no recovery notice must be emitted for voided (not real) evidence"
    );

    shutdown(handle, join).await;
}

// ── 20: `[notifications].enabled = false` → zero sink calls (T4 Must #3) ───
//
// `notifier::spawn` returns `None` when `cfg.enabled` is `false` (unit-level
// coverage lives in `notifier::tests::spawn_returns_none_when_notifications_disabled`).
// This is the daemon-level companion: with the notifier disabled, a wake
// failure burst well past the (disabled) threshold must never reach the
// injected `RecordingSink` at all — because no notifier task exists to call
// it, not merely because policy suppressed the send.

fn notifier_disabled_config(marker: &Path, wake_cmd: &str, grace: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[notifications]
enabled = false
wake_attempt_threshold = 2
cooldown = "1m"

[displays.mon]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m}'"
wake_command = "{wake_cmd}"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m = marker.display(),
        wake_cmd = wake_cmd,
        g = grace,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifier_disabled_records_zero_sink_calls_after_failure_burst() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");

    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &notifier_disabled_config(&marker, "false", "20ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // Same absent→present script as test 19, driving several `wake_command
    // = "false"` failures well past `wake_attempt_threshold = 2` — the only
    // difference is `[notifications].enabled = false`.
    let script = vec![
        (Duration::from_millis(0), ev("desk", SensorState::Absent)),
        (Duration::from_millis(150), ev("desk", SensorState::Present)),
    ];

    let sink = std::sync::Arc::new(RecordingSink::default());
    let sink_for_builder = sink.clone();
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .disable_ipc()
    .with_notify_sink_builder(move || sink_for_builder.clone() as std::sync::Arc<dyn NotifySink>);
    let (handle, join) = app.start().await.expect("start app");

    // Let several wake-retry cycles elapse — plenty of time for a failure
    // burst that would, if the notifier were enabled, easily cross the
    // threshold and produce at least one `Send`.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        sink.notifies.lock().unwrap().is_empty(),
        "enabled=false: NotifySink::notify must never be called"
    );
    assert!(
        sink.closes.lock().unwrap().is_empty(),
        "enabled=false: NotifySink::close must never be called"
    );

    shutdown(handle, join).await;
}

// ── 21: reload carries sensor `reported` until the sensor's own config edit (T4) ──
//
// `.disable_ipc()`. Mirrors test 18's shape (carry-over across an unrelated
// edit, then voiding on a dispatch-relevant edit) but for the sensor-side
// `reported` diagnostic bit instead of display `last_blank_failed`. The
// sensor's own `topic` changing is the dispatch-relevant edit here — any
// whole-`SensorConfig` difference zeroes `reported` (spec R3-S), unlike the
// per-field display gate.

/// One `mqtt` sensor ("desk") feeding an `any` zone that drives a quiet
/// rule-driven display; `log_level` and `topic` are the two knobs this test
/// flips independently (unrelated daemon-level edit vs. a sensor-own-config
/// edit). `grace_period` is long so no dispatch confounds the `reported`
/// assertions within the test window.
fn reported_carry_config(topic: &str, log_level: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"
log_level = "{log_level}"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "{topic}"

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
grace_period = "10s"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes this test's reload() calls against the exact- \
              count step-boundary tests they would otherwise contaminate — test-local, always \
              released promptly at test end"
)]
async fn reload_carries_sensor_reported_until_own_config_edit() {
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &reported_carry_config("zigbee2mqtt/desk", "info"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // No scripted events — the sensor is driven live via `events_sender()`
    // so the test controls exactly when the first event lands.
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();
    let ctl = handle.control_sender();

    // Before any event, `reported` must read false (fail-safe seed).
    let snap0 = snapshot_with_retry(&ctl).await;
    let desk0 = snap0
        .sensors
        .iter()
        .find(|s| s.id == "desk")
        .expect("desk sensor in snapshot");
    assert!(!desk0.reported, "reported must be false before any event");

    // Deliver one live event — `reported` must flip true.
    handle
        .events_sender()
        .send(ev("desk", SensorState::Present))
        .await
        .unwrap();
    assert!(
        wait_for_sensor_reported(&ctl, "desk", true, Duration::from_secs(3)).await,
        "reported must flip true after the first event"
    );

    // ── Reload #1: unrelated daemon-level edit — same sensor block ─────
    fs::write(
        &cfg_path,
        reported_carry_config("zigbee2mqtt/desk", "debug"),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload #1 outcome in time")
        .expect("reload bus open");
    assert_eq!(
        outcome,
        ReloadOutcome::Reloaded,
        "reload #1 must be accepted"
    );

    assert!(
        wait_for_sensor_reported(&ctl, "desk", true, Duration::from_secs(3)).await,
        "unrelated config edit must carry reported=true forward"
    );

    // ── Reload #2: sensor's own config edit (topic change) ─────────────
    fs::write(
        &cfg_path,
        reported_carry_config("zigbee2mqtt/desk-NEW", "debug"),
    )
    .unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload #2 outcome in time")
        .expect("reload bus open");
    assert_eq!(
        outcome,
        ReloadOutcome::Reloaded,
        "reload #2 must be accepted"
    );

    assert!(
        wait_for_sensor_reported(&ctl, "desk", false, Duration::from_secs(2)).await,
        "sensor's own config edit must void (zero) the carried reported bit"
    );

    shutdown(handle, join).await;
}

// ── 22: absent-policy + mqtt hazard warns at startup, silent on rejection (T4) ──
//
// `.disable_ipc()`. Uses the file's tracing-capture precedent
// (`install_capture_subscriber`/`drain_capture`, tests 10/11). Per the P5
// race caveat: the capture buffer is process-global, so (a) the absence
// window is bounded by draining immediately before the rejected reload and
// reading immediately after, and (b) the zone/sensor names are unique to
// this test so sibling tests running concurrently in the same process
// cannot pollute the evidence either direction.

/// One `mqtt` sensor as a direct member of an `unavailable_policy = "absent"`
/// zone — the exact hazard shape `absent_mqtt_hazards` matches on. No
/// displays/rules are needed for the hazard warn itself, but a quiet
/// rule-driven display is included anyway to keep the config shape close to
/// the file's other fixtures.
fn hazard_config(zone: &str, sensor: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.{sensor}]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "t"

[zones.{zone}]
mode = "any"
members = ["{sensor}"]
unavailable_policy = "absent"

[displays.mon]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "true"
wake_command = "true"
modes = ["power_off"]

[rules.r]
zone = "{zone}"
displays = ["mon"]
grace_period = "10s"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_mqtt_hazard_warns_at_startup_and_not_on_rejected_reload() {
    const ZONE: &str = "hazmat-zone-t4";
    const SENSOR: &str = "hazmat-sensor-t4";

    install_capture_subscriber();

    let dir = TempDir::new().unwrap();
    let cfg_path = write_file(dir.path(), "config.toml", &hazard_config(ZONE, SENSOR));
    let creds_path = dir.path().join("credentials.toml");

    drain_capture(); // discard any startup noise from prior tests

    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory(SENSOR, Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let startup_output = drain_capture();
    assert!(
        startup_output.contains("unavailable_absent_mqtt")
            && startup_output.contains(ZONE)
            && startup_output.contains(SENSOR),
        "expected unavailable_absent_mqtt hazard warn at startup mentioning zone/sensor: \
         {startup_output}"
    );

    let mut reloads = handle.subscribe_reload();

    // Bound the absence window: drain immediately before the rejected
    // reload, read immediately after (P5 race caveat).
    drain_capture();
    fs::write(&cfg_path, "config_version = 1\nbogus = true\n").unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    match outcome {
        ReloadOutcome::Rejected(_) => {}
        ReloadOutcome::Reloaded => panic!("expected Rejected, got Reloaded"),
    }
    let rejected_output = drain_capture();
    assert!(
        !rejected_output.contains("unavailable_absent_mqtt"),
        "a REJECTED reload must not log a new unavailable_absent_mqtt hazard warn: \
         {rejected_output}"
    );

    shutdown(handle, join).await;
}

// ── Watchdog probe arm + LKG promotion (T4) ─────────────────────────────────
//
// `.disable_ipc()` throughout (Global Constraints). `XDG_STATE_HOME` is
// process-global, so every test below serializes on `wear_env_lock()` (the
// SAME lock the wear tests already use for the same env var — a single
// dedicated lock per env var, not one per feature). `WATCHDOG_USEC` is
// NEVER touched (Global Constraints); cadence is controlled entirely via
// `App::with_watchdog_interval` + `SdNotify::from_socket_for_test`
// (`test-util` feature — see `Cargo.toml`/`sd_notify.rs` T4 fix).
//
// Timing (reported in the T4 write-up): `stability_window = "30s"` is the
// validated floor (`validate.rs`); `watchdog_healthy_run_writes_lkg_and_sidecar`
// and `watchdog_reload_mid_window_resets_candidate` each wait close to that
// floor and are run with their OWN `timeout 42` invocation, never inside
// either of the two bulk runs below, to stay inside the 42s harness budget.
//
// F5 (T4 review): once this file grew enough `watchdog_*` step-boundary
// tests, running "every test except the two long ones" as a single
// `--test-threads=4` invocation risked busting the 42s cap the moment any
// more watchdog coverage landed. The fix is a STABLE two-half split by
// name, not a shrinking act — CI, reviewers, and operators re-running this
// file locally all use the exact same two commands:
//
//   1) watchdog smoke half (all `watchdog_*` tests EXCEPT the two long
//      ones above, which keep their own individual `timeout 42` runs):
//        timeout 42 cargo test -p dormantd --test daemon_smoke \
//          --all-features -- --test-threads=4 watchdog_ \
//          --skip watchdog_healthy_run_writes_lkg_and_sidecar \
//          --skip watchdog_reload_mid_window_resets_candidate
//      Measured on the sandbox's 2-core box: ~2.7s.
//
//   2) everything else (every non-`watchdog_*` test in this file —
//      `--skip watchdog_` substring-matches and excludes ALL of them,
//      including the two long ones, so this half never touches them):
//        timeout 42 cargo test -p dormantd --test daemon_smoke \
//          --all-features -- --test-threads=4 --skip watchdog_
//      Measured on the sandbox's 2-core box: ~40-41s across repeated runs
//      — inside the 42s cap but with LESS headroom than half (1). This is
//      a PRE-EXISTING characteristic of the legacy (pre-T4) test bucket,
//      not a regression from this fix: none of the tests added by this
//      commit land in half (2), and its runtime is unchanged from before
//      this branch. A real-time-heavy handful (e.g.
//      `wear_park_persists_final_ledger`'s fixed 7s settle sleep) account
//      for most of the margin loss; rebalancing that legacy weight is
//      out of scope here — flagged as a follow-up, not fixed in this
//      watchdog-focused change.
//
// A new smoke test belongs in half (1) if its name starts with
// `watchdog_`, half (2) otherwise — no other bookkeeping needed as the
// suite keeps growing. A new HEAVY (multi-second real sleep) test in half
// (2) should be weighed against that shrinking margin before it lands.

use dormantd::sd_notify::SdNotify;

/// `install_capture_subscriber`'s buffer is process-global and, once
/// installed, becomes the default subscriber for EVERY test in this
/// binary (not just ones that call it) — the existing hazard test (22)
/// documents the residual race and bounds its window instead of trying to
/// eliminate it entirely.
///
/// The three EXACT-COUNT / ordered step-boundary tests
/// (`watchdog_in_reload_pings_healthy_boundaries`,
/// `watchdog_ping_before_rebuild_old_on_verified_wake_failure`,
/// `watchdog_ping_before_rebuild_old_on_spawn_generation_failure`) are far
/// more sensitive to that race than a substring check: `Runner::reload()`
/// logs the SAME `after_assemble`/`after_quiesce`/`before_teardown`/
/// `removed_display_wake`/`reload_end` step markers (`app.rs`'s
/// `Runner::ping`) on every single reload it runs, whether or not the
/// caller cares about watchdog pings — so any OTHER test in this binary
/// that drives a real `Runner::reload()` (`trigger_reload()` or a file-
/// watcher-triggered reload) while a step-boundary test's capture window
/// is open can inject a foreign marker into the count/order the reader is
/// asserting on.
///
/// Mirrors `boot_rollback.rs`'s fix for the identical mechanism there
/// (`config_rollback_boot`): every reload-driving test in this binary that
/// can emit these markers takes this lock for its own reload work, not
/// just the readers — a non-emitting test needs no lock (its own log
/// lines never match the readers' substrings). This trades away
/// cross-test parallelism for these particular tests in exchange for
/// eliminating the collision outright; the binary has ample real-time
/// headroom (see the two-half split above) to absorb it.
static CAPTURE_COUNT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_count_lock() -> &'static Mutex<()> {
    CAPTURE_COUNT_LOCK.get_or_init(|| Mutex::new(()))
}

/// [`one_display_config`] plus a `[watchdog]` section.
fn watchdog_config(marker: &Path, stability_window: &str, lkg_enabled: bool) -> String {
    format!(
        "{}\n[watchdog]\nlkg_enabled = {lkg_enabled}\nstability_window = \"{stability_window}\"\n",
        one_display_config(marker, "0s"),
    )
}

/// Bind a fresh `UnixDatagram` "systemd" listener at a tempdir path and
/// build an `SdNotify` targeting it (the `from_socket_for_test` seam,
/// R2-M8/T4 fix).
fn fake_systemd_socket(dir: &Path) -> (std::os::unix::net::UnixDatagram, SdNotify) {
    let path = dir.join("notify.sock");
    let listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
    let addr = std::os::unix::net::SocketAddr::from_pathname(&path).unwrap();
    let sd = SdNotify::from_socket_for_test(&addr);
    (listener, sd)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock + capture_count_lock serialize test-local state and are \
              always released promptly at test end"
)]
async fn watchdog_healthy_run_writes_lkg_and_sidecar() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &watchdog_config(&marker, "30s", true),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (_listener, sd) = fake_systemd_socket(dir.path());

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .with_sd_notify(sd)
    .with_watchdog_interval(Duration::from_millis(500));
    let (handle, join) = app.start().await.expect("start app");

    let lkg_path = state_home
        .path()
        .join("dormant")
        .join("last-known-good.toml");
    let meta_path = state_home
        .path()
        .join("dormant")
        .join("last-known-good.meta.json");
    let ok = wait_for(
        || lkg_path.exists() && meta_path.exists(),
        Duration::from_secs(35),
    )
    .await;

    shutdown(handle, join).await;

    assert!(
        ok,
        "last-known-good.toml + .meta.json must appear after a stable window"
    );
    let lkg_bytes = fs::read(&lkg_path).unwrap();
    assert!(
        !lkg_bytes.is_empty(),
        "LKG copy must be a non-empty verbatim config copy"
    );
    let meta = read(&meta_path);
    assert!(
        meta.contains("\"source\": \"boot\"") || meta.contains("\"source\": \"reload\""),
        "the first candidate's sidecar source must be \"boot\" or \"reload\": {meta}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes XDG_STATE_HOME-touching tests across the whole test \
              body (app lifetime included) — test-local, always released promptly at test end"
)]
async fn watchdog_lkg_disabled_writes_nothing() {
    let _guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &watchdog_config(&marker, "30s", false),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (_listener, sd) = fake_systemd_socket(dir.path());

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .with_sd_notify(sd)
    .with_watchdog_interval(Duration::from_millis(100));
    let (handle, join) = app.start().await.expect("start app");

    // Several ticks' worth of real time — long enough that a buggy
    // "tracks anyway" implementation would have written something.
    tokio::time::sleep(Duration::from_secs(2)).await;
    shutdown(handle, join).await;

    let lkg_path = state_home
        .path()
        .join("dormant")
        .join("last-known-good.toml");
    assert!(
        !lkg_path.exists(),
        "watchdog.lkg_enabled = false must never write last-known-good.toml"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "wear_env_lock serializes XDG_STATE_HOME-touching tests across the whole test \
              body (app lifetime included) — test-local, always released promptly at test end"
)]
async fn watchdog_reload_mid_window_resets_candidate() {
    let _wear_guard = wear_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Serialize against the count-sensitive watchdog tests: this test does
    // a reload which emits the same step-boundary markers
    // (after_assemble/after_quiesce/before_teardown/reload_end) that
    // watchdog_in_reload_pings_healthy_boundaries counts — without this
    // lock, a parallel reload from this test contaminates that count.
    let _capture_guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state_home = TempDir::new().unwrap();
    let _env = XdgStateHomeGuard::set(state_home.path());

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &watchdog_config(&marker, "30s", true),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (_listener, sd) = fake_systemd_socket(dir.path());

    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .with_sd_notify(sd)
    .with_watchdog_interval(Duration::from_millis(500));
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    // Reload well inside the 30s window (same watchdog settings — a valid,
    // accepted reload) — this must reset the candidate's window start.
    tokio::time::sleep(Duration::from_secs(3)).await;
    fs::write(&cfg_path, watchdog_config(&marker, "30s", true)).unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    // From the reload, wait LESS than the full 30s window: if the reload
    // had NOT reset the candidate, the original (pre-reload) candidate
    // would already be > 30s old by now and would have promoted.
    tokio::time::sleep(Duration::from_secs(24)).await;

    let lkg_path = state_home
        .path()
        .join("dormant")
        .join("last-known-good.toml");
    let premature = lkg_path.exists();

    shutdown(handle, join).await;

    assert!(
        !premature,
        "a reload mid-window must reset the LKG candidate — no premature promotion"
    );
}

/// Three-display config where `mon2`/`mon3` can be dropped from the rule
/// (and, via `include_displays`, from `[displays]` entirely) on reload —
/// enough removed displays to distinguish "one ping per removed display"
/// from "one ping for the whole loop" (P9).
fn three_display_config(
    m1: &Path,
    m2: &Path,
    m3: &Path,
    wake2: &str,
    wake3: &str,
    include_displays: bool,
    include_in_rule: bool,
) -> String {
    let extra_displays = if include_displays {
        format!(
            r#"
[displays.mon2]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m2}'"
wake_command = "{w2}"
modes = ["power_off"]

[displays.mon3]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m3}'"
wake_command = "{w3}"
modes = ["power_off"]
"#,
            m2 = m2.display(),
            w2 = wake2,
            m3 = m3.display(),
            w3 = wake3,
        )
    } else {
        String::new()
    };
    let displays = if include_in_rule {
        r#"["mon1", "mon2", "mon3"]"#
    } else {
        r#"["mon1"]"#
    };
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "x"

[zones.office]
mode = "any"
members = ["desk"]

[displays.mon1]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "printf B >> '{m1}'"
wake_command = "printf W >> '{m1}'"
modes = ["power_off"]
{extra_displays}
[rules.r]
zone = "office"
displays = {displays}
grace_period = "120ms"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        m1 = m1.display(),
    )
}

// Linux-only: datagram-count assertion on the systemd-only ping arm —
// see the cadence test's note above.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes every reload-driving test in this binary against \
              this exact-count reader (see the lock's doc comment) and is always released \
              promptly at test end"
)]
#[allow(
    clippy::too_many_lines,
    reason = "F2: exact-count + strict-ordering assertions across seven step-boundary markers \
              read better linearly than split across helper fns for a single-purpose test"
)]
async fn watchdog_in_reload_pings_healthy_boundaries() {
    let _guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();

    let dir = TempDir::new().unwrap();
    let m1 = dir.path().join("mon1");
    let m2 = dir.path().join("mon2");
    let m3 = dir.path().join("mon3");
    let wake2 = format!("printf W >> '{}'", m2.display());
    let wake3 = format!("printf W >> '{}'", m3.display());
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &three_display_config(&m1, &m2, &m3, &wake2, &wake3, true, true),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (_listener, sd) = fake_systemd_socket(dir.path());

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .with_sd_notify(sd)
    // Long interval — the periodic tick arm must not fire during this
    // short test, so every "watchdog_ping" marker observed is attributable
    // to the in-reload step boundaries alone (P9's per-step, not
    // aggregate, requirement).
    .with_watchdog_interval(Duration::from_secs(120));
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(
            || count(&m1, 'B') >= 1 && count(&m2, 'B') >= 1 && count(&m3, 'B') >= 1,
            Duration::from_secs(3)
        )
        .await,
        "all three displays should blank before reload"
    );

    drain_capture(); // discard startup noise

    // Drop mon2 + mon3 (both removed + dark): the verified-wake loop must
    // ping once per removed display, plus the shared before-teardown and
    // reload-end boundaries.
    // Rely SOLELY on the file watcher (no `handle.trigger_reload()`): the
    // IPC trigger and the watcher fire on independent channels, and
    // `debounce()` only drains the watcher's — pairing both here would
    // fire a SECOND (no-op) reload after the first, double-counting every
    // step-boundary marker below.
    fs::write(
        &cfg_path,
        three_display_config(&m1, &m2, &m3, &wake2, &wake3, false, false),
    )
    .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    let output = drain_capture();
    shutdown(handle, join).await;

    // NOTE: `tracing_subscriber::fmt`'s default ANSI colouring inserts
    // escape codes BETWEEN the `step` key, `=`, and its value, so a
    // literal "step=before_teardown" substring never matches the raw
    // captured bytes even though it renders contiguously — match on the
    // (sufficiently distinctive) bare step-name token instead.
    assert_eq!(
        output.matches("after_assemble").count(),
        1,
        "after_assemble must fire exactly once (spec §6.3: distinct from after_quiesce): {output}"
    );
    assert_eq!(
        output.matches("after_quiesce").count(),
        1,
        "after_quiesce must fire exactly once (spec §6.3: distinct from after_assemble): {output}"
    );
    assert_eq!(
        output.matches("before_teardown").count(),
        1,
        "before_teardown must fire exactly once: {output}"
    );
    assert_eq!(
        output.matches("removed_display_wake").count(),
        2,
        "removed_display_wake must fire once PER removed display (2 removed), not once for \
         the whole loop: {output}"
    );
    assert_eq!(
        output.matches("reload_end").count(),
        1,
        "reload_end must fire exactly once on a successful reload: {output}"
    );

    // Order matters as much as presence (spec §6.3 names these as an
    // ordered sequence of boundaries): after_assemble -> after_quiesce ->
    // before_teardown -> removed_display_wake (xN) -> reload_end.
    let pos = |needle: &str| {
        output
            .find(needle)
            .unwrap_or_else(|| panic!("marker {needle} missing from captured output: {output}"))
    };
    let p_assemble = pos("after_assemble");
    let p_quiesce = pos("after_quiesce");
    let p_teardown = pos("before_teardown");
    let p_wake_first = pos("removed_display_wake");
    let p_reload_end = pos("reload_end");
    assert!(
        p_assemble < p_quiesce,
        "after_assemble must fire before after_quiesce: {output}"
    );
    assert!(
        p_quiesce < p_teardown,
        "after_quiesce must fire before before_teardown: {output}"
    );
    assert!(
        p_teardown < p_wake_first,
        "before_teardown must fire before removed_display_wake: {output}"
    );
    assert!(
        p_wake_first < p_reload_end,
        "removed_display_wake must fire before reload_end: {output}"
    );
}

// Linux-only: datagram-count assertion on the systemd-only ping arm —
// see the cadence test's note above.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes every reload-driving test in this binary against \
              this exact-count reader (see the lock's doc comment) and is always released \
              promptly at test end"
)]
async fn watchdog_ping_before_rebuild_old_on_verified_wake_failure() {
    let _guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();

    let dir = TempDir::new().unwrap();
    let m1 = dir.path().join("mon1");
    let m2 = dir.path().join("mon2");
    let m3 = dir.path().join("mon3");
    // mon3's wake command always fails — the verified-wake loop must abort
    // on it (after successfully pinging for mon2, processed first).
    let wake2 = format!("printf W >> '{}'", m2.display());
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &three_display_config(&m1, &m2, &m3, &wake2, "false", true, true),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (_listener, sd) = fake_systemd_socket(dir.path());

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .with_sd_notify(sd)
    .with_watchdog_interval(Duration::from_secs(120));
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(
            || count(&m1, 'B') >= 1 && count(&m2, 'B') >= 1 && count(&m3, 'B') >= 1,
            Duration::from_secs(3)
        )
        .await,
        "all three displays should blank before reload"
    );

    drain_capture();

    // Watcher-only trigger — see the sibling test's comment on why pairing
    // this with `handle.trigger_reload()` would double-fire.
    fs::write(
        &cfg_path,
        three_display_config(&m1, &m2, &m3, &wake2, "false", false, false),
    )
    .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    match outcome {
        ReloadOutcome::Rejected(_) => {}
        ReloadOutcome::Reloaded => panic!("expected Rejected (mon3 verified wake fails)"),
    }

    let output = drain_capture();
    shutdown(handle, join).await;

    // See the sibling test's note: matching on the bare step-name token,
    // not "step=X" (ANSI escape codes split the key/value pair in the raw
    // captured bytes).
    assert_eq!(
        output.matches("before_teardown").count(),
        1,
        "before_teardown must still fire: {output}"
    );
    assert!(
        output.contains("before_rebuild_old_wake_failure"),
        "the rebuild_old(wake-failure) call site must be pinged first: {output}"
    );
    assert!(
        !output.contains("reload_end"),
        "an aborted reload must never reach the reload_end boundary: {output}"
    );
}

/// F1 (T4 review): the `before_rebuild_old_spawn_failure` boundary (the
/// ACCEPTED-config `spawn_generation` call failing, distinct from the
/// verified-wake failure covered by the sibling test above) had NO
/// coverage. No config-only edit can drive a REAL `spawn_generation`
/// failure past `validate()` here — `ZoneEngine::new` runs the identical
/// deterministic construction `validate()` already ran on the same `cfg`,
/// and `RulesEngine::new`'s only fallible check can't fire because
/// `assemble_static` is fail-fast (an incomplete executor/machine map never
/// reaches `spawn_generation`). This test uses the `App::
/// with_test_force_reload_spawn_failure` seam (test-util feature, same
/// pattern as `SdNotify::from_socket_for_test`) to force the REAL `Err(e)`
/// arm in `Runner::reload` to run, with a synthetic cause — everything
/// downstream of that point (the ping, `rebuild_old`, the `Rejected`
/// outcome) is production code, unmodified.
// Linux-only: datagram-count assertion on the systemd-only ping arm —
// see the cadence test's note above.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::await_holding_lock,
    reason = "capture_count_lock() serializes every reload-driving test in this binary against \
              this exact-count reader (see the lock's doc comment) and is always released \
              promptly at test end"
)]
async fn watchdog_ping_before_rebuild_old_on_spawn_generation_failure() {
    let _guard = capture_count_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    install_capture_subscriber();

    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "0s"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let (_listener, sd) = fake_systemd_socket(dir.path());

    let script = vec![(Duration::from_millis(100), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc()
    .with_sd_notify(sd)
    .with_watchdog_interval(Duration::from_secs(120))
    .with_test_force_reload_spawn_failure();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();

    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "display should blank before reload"
    );

    drain_capture(); // discard startup noise

    // Any accepted-shape edit triggers a reload attempt; the seam forces
    // spawn_generation to fail regardless of content.
    fs::write(&cfg_path, one_display_config(&marker, "50ms")).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    match outcome {
        ReloadOutcome::Rejected(_) => {}
        ReloadOutcome::Reloaded => panic!("expected Rejected (forced spawn_generation failure)"),
    }

    let output = drain_capture();
    shutdown(handle, join).await;

    assert!(
        output.contains("before_rebuild_old_spawn_failure"),
        "the accepted-spawn failure boundary must fire: {output}"
    );
    assert!(
        !output.contains("reload_end"),
        "an aborted reload must never reach the reload_end boundary: {output}"
    );
}

/// A failed accepted-config spawn leaves both front-door routers paused while
/// `rebuild_old` attempts to restore service. If that second spawn fails too,
/// the daemon must exit rather than remain alive without an engine that can
/// process a wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_double_spawn_failure_cancels_root() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_test_force_reload_spawn_failure()
    .with_test_force_rebuild_old_spawn_failure()
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    fs::write(&cfg_path, one_display_config(&marker, "50ms")).unwrap();
    let (request_id, receipt) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("rejected reload receipt");
    assert!(receipt.request_ids.contains(&request_id));
    assert!(matches!(receipt.outcome, ReloadOutcome::Rejected(_)));
    assert!(
        handle.root_is_cancelled_for_test(),
        "double spawn failure must cancel the daemon root token"
    );
    tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .expect("cancelled root must stop the run loop")
        .expect("run loop must not panic");
}

/// Pausing the front-door routers must not close the old engine before its
/// drain barrier arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_barrier_survives_router_pause() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let gate = GenerationBarrierGate::new();
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_test_generation_barrier_gate(gate.clone())
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let (snapshot_tx, snapshot_rx) = oneshot::channel();
    handle
        .control_sender()
        .send(ControlMsg::Snapshot(snapshot_tx))
        .await
        .expect("old generation control route stays open");
    tokio::time::timeout(Duration::from_secs(1), snapshot_rx)
        .await
        .expect("old generation responds before the forced barrier timeout")
        .expect("old generation snapshot reply");

    fs::write(&cfg_path, one_display_config(&marker, "50ms")).unwrap();
    {
        let reload = handle.request_reload_with_id(ReloadSource::Control);
        tokio::pin!(reload);
        tokio::select! {
            _ = &mut reload => panic!("reload completed before reaching the old-generation barrier"),
            result = tokio::time::timeout(Duration::from_secs(1), gate.wait_until_entered()) => {
                assert!(result.is_ok(), "reload did not reach the old-generation barrier");
            }
        }
        let probe = gate.request_post_release_snapshot();
        gate.release();
        let old_snapshot = tokio::time::timeout(Duration::from_secs(1), probe)
            .await
            .expect("old engine accepted the post-release barrier probe")
            .expect("reload kept the barrier probe alive")
            .expect("old engine control route remained open after pause");
        tokio::time::timeout(Duration::from_secs(1), old_snapshot)
            .await
            .expect("old engine answered the post-release barrier probe")
            .expect("old engine snapshot reply");
        let (request_id, receipt) = tokio::time::timeout(Duration::from_secs(3), reload)
            .await
            .expect("reload completes after the barrier")
            .expect("reload requester remains available");
        assert!(receipt.request_ids.contains(&request_id));
        assert_eq!(receipt.outcome, ReloadOutcome::Reloaded);
        assert!(!handle.root_is_cancelled_for_test());
    }
    shutdown(handle, join).await;
}

/// An engine that fails to acknowledge its drain barrier must hand recovery to
/// the supervisor rather than leave paused routers and a reload request wedged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_barrier_timeout_cancels_root() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_test_force_generation_barrier_timeout()
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    fs::write(
        &cfg_path,
        one_display_config(&marker, "50ms").replace(
            "startup_holdoff = \"0s\"",
            "startup_holdoff = \"0s\"\nreload_debounce = \"0s\"\ngeneration_barrier_ack_timeout = \"50ms\"",
        ),
    )
    .unwrap();
    let reload_started = Instant::now();
    let (request_id, receipt) = tokio::time::timeout(
        Duration::from_secs(3),
        handle.request_reload_with_id(ReloadSource::Control),
    )
    .await
    .expect("barrier failure produces a receipt")
    .expect("reload requester remains available");
    assert!(receipt.request_ids.contains(&request_id));
    assert!(
        matches!(&receipt.outcome, ReloadOutcome::Rejected(detail) if detail.contains("timeout")),
        "barrier failure must report an acknowledgement timeout: {:?}",
        receipt.outcome
    );
    assert!(
        reload_started.elapsed() >= Duration::from_millis(50),
        "forced missing acknowledgement must exercise the bounded wait"
    );
    assert!(
        handle.root_is_cancelled_for_test(),
        "missing generation-barrier acknowledgement must cancel the daemon root"
    );
    tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .expect("cancelled root stops the run loop")
        .expect("run loop must not panic");
}

/// Rollback-recovery plan Task 1 §8: `rebuild_old` must carry
/// Runner-OWNED rollback status (`self.rollback_status.clone()`) even when
/// the preliminary snapshot request timed out — `rebuild_old` never derives
/// rollback presentation from `snapshot.and_then(...)`, which would be
/// `None` here by construction (`with_test_force_reload_snapshot_timeout`).
/// Combined with `with_test_force_reload_spawn_failure` to force the
/// accepted-spawn `Err` arm (so this reload actually reaches
/// `rebuild_old`), matching the sibling test above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_old_after_snapshot_timeout_preserves_runner_rollback() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let status = RollbackStatus {
        failed_fp: "12:deadbeef".to_string(),
        lkg_fp: "11:cafebabe".to_string(),
        detail: "running last-known-good".to_string(),
    };
    let lifecycle = ReloadLifecycleCapture::new();
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_test_rollback_status(status.clone(), dir.path().to_path_buf())
    .with_test_force_reload_snapshot_timeout()
    .with_test_force_reload_spawn_failure()
    .with_test_reload_lifecycle_capture(lifecycle.clone())
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    handle
        .control_sender()
        .send(ControlMsg::SetRollback(Some(status.clone())))
        .await
        .expect("park boot rollback for the direct-App test");
    let mut reloads = handle.subscribe_reload();

    std::fs::write(&cfg_path, one_display_config(&marker, "100ms")).unwrap();
    assert!(handle.trigger_reload().await);
    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .unwrap_or_else(|elapsed| {
            panic!(
                "reload outcome: {elapsed:?}; lifecycle stages: {:?}",
                lifecycle.stages()
            )
        })
        .expect("reload bus");
    assert!(matches!(outcome, ReloadOutcome::Rejected(_)));

    let snapshot = snapshot_with_retry(&handle.control_sender()).await;
    assert_eq!(snapshot.rollback, Some(status));
    shutdown(handle, join).await;
}

/// A watcher request can arrive after a rejected reload has restored the old
/// generation but before the operator changes the still-rejected revision.
/// Each execution must drain and rebuild independently rather than leaving
/// the routers paused behind the first rollback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_reload_can_repeat_after_rollback_restore() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &one_display_config(&marker, "400ms"),
    );
    let creds_path = dir.path().join("credentials.toml");
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", Vec::new()),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_test_force_reload_snapshot_timeout()
    .with_test_force_reload_spawn_failure()
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    fs::write(&cfg_path, one_display_config(&marker, "100ms")).unwrap();

    for source in [ReloadSource::Watcher, ReloadSource::Control] {
        let (request_id, receipt) = tokio::time::timeout(
            Duration::from_secs(3),
            handle.request_reload_with_id(source),
        )
        .await
        .expect("rejected reload receipt in time")
        .expect("reload requester remains available");
        assert!(receipt.request_ids.contains(&request_id));
        assert!(matches!(receipt.outcome, ReloadOutcome::Rejected(_)));
    }

    assert!(
        !handle.root_is_cancelled_for_test(),
        "a successful rollback keeps the daemon serving a later rejected revision"
    );
    shutdown(handle, join).await;
}

// ── T6: audio/call inhibitor — first inhibitor smoke coverage ──────────────────
//
// (a) grace-freeze: a running stream keeps the display's grace countdown
//     frozen past its own expiry; swapping to an idle fixture unfreezes it
//     and the blank proceeds (`state_machine.rs:1213-1237`'s full-remaining
//     pre-freeze when the inhibitor is already asserted at `Grace` entry).
// (b) reload-mid-movie (spec F2/R2-M1, anti-tautology per plan P4):
//     `min_active = "5s"` is set STRICTLY greater than the post-reload
//     observation window (`grace = "3s"`), so ordinary debounce cannot
//     re-freeze the new generation's display in time — only the FRESH
//     `startup_grace` exemption on the new poller's first successful tick
//     can. A missing exemption lets the zone's own live Absent edge run an
//     unfrozen grace countdown that expires before ordinary debounce would
//     ever assert, and the display blanks — visibly RED.

const AUDIO_MOVIE_FIXTURE: &str = include_str!("fixtures/pw_dump/movie.json");
const AUDIO_IDLE_FIXTURE: &str = include_str!("fixtures/pw_dump/idle.json");

/// Wrap a raw `pw-dump` JSON fixture in a `#!/bin/sh` script that `cat`s it
/// verbatim (the `audio_source.rs` poller-test precedent, same mechanism).
fn audio_cat_script(json: &str) -> String {
    format!("#!/bin/sh\ncat <<'AUDIO_FIXTURE_EOF'\n{json}\nAUDIO_FIXTURE_EOF\n")
}

/// Write a fake `pw-dump` script emitting `json`, executable (0o755 — the
/// `write_credentials` `PermissionsExt` precedent above; same mechanism,
/// 0o755 not 0o600 because this file must be executable).
fn write_pw_dump_script(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("pw-dump");
    fs::write(&path, audio_cat_script(json)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Atomically swap a running fake `pw-dump` script's fixture output:
/// write-temp + `chmod` 0o755 BEFORE rename — rename carries the temp's
/// mode, so losing the exec bit would silently flip the poller onto its
/// failure path (R2-N, `audio_source.rs`'s `rewrite_script` precedent).
fn rewrite_pw_dump_script(path: &Path, json: &str) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, audio_cat_script(json)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::rename(&tmp, path).unwrap();
}

/// One ruled `command` display opting into `inhibitors = ["audio-playback"]`,
/// with a global `[audio]` section pointed at the fake `pw-dump` script.
/// `poll_interval` is fixed at `"1s"` (P3 — the validation floor is `>= 1s`);
/// `grace`/`min_active` are tunable per case (P3/P4); `log_level` is the
/// unrelated daemon-level knob the reload test flips (`reload_carry_config`
/// precedent above).
fn audio_rule_config(
    marker: &Path,
    script: &Path,
    grace: &str,
    min_active: &str,
    log_level: &str,
) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"
log_level = "{log_level}"

[audio]
poll_interval = "1s"
min_active = "{min_active}"
pw_dump_command = "{script}"

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
blank_command = "printf B >> '{m}'"
wake_command = "printf W >> '{m}'"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "{g}"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
inhibitors = ["audio-playback"]
"#,
        log_level = log_level,
        min_active = min_active,
        script = script.display(),
        m = marker.display(),
        g = grace,
    )
}

// ── (a) grace-freeze then blank on idle ─────────────────────────────────────────

// Linux-only: full-daemon audio integration over a fake pw-dump script —
// PipeWire is a Linux subsystem (macOS audio-aware blanking is out of
// scope by design; production there fail-safes via the spawn-failure
// breaker), and the test's grace/timing windows break under macos-latest
// spawn latency (PR #78 round-9 rerun).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_playback_freezes_grace_past_expiry_then_blanks_on_idle() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let script = write_pw_dump_script(dir.path(), AUDIO_MOVIE_FIXTURE);
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &audio_rule_config(&marker, &script, "3s", "0s", "info"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // The zone's own `Absent` edge is delayed well past the poller's first
    // (immediate-at-spawn) tick, so `enter_grace` always observes the
    // inhibitor already asserted and pre-freezes with the FULL grace period
    // (`state_machine.rs:1213-1237`) — deterministic regardless of tick
    // jitter, rather than racing a partial freeze.
    let sensor_script = vec![(Duration::from_millis(800), ev("desk", SensorState::Absent))];
    let observations = ObservationHub::new(64);
    let mut observation_rx = observations.subscribe();
    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk", sensor_script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_observation_hub(observations)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation zero entering grace",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(0),
                    rule_id: Some(rule_id),
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Active,
                    new_phase: dormant_core::state_machine::Phase::Grace { .. },
                } if rule_id.0 == "r" && display_id.0 == "mon"
            )
        },
    )
    .await;
    assert!(
        wait_for_display_inhibited(
            &handle.control_sender(),
            "mon",
            true,
            Duration::from_secs(3),
        )
        .await,
        "movie fixture must inhibit the display while grace is active"
    );

    rewrite_pw_dump_script(&script, AUDIO_IDLE_FIXTURE);
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(6),
        "generation zero blanking completed after audio deassertion",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(0),
                    rule_id: Some(rule_id),
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Blanking,
                    new_phase: dormant_core::state_machine::Phase::Blanked,
                } if rule_id.0 == "r" && display_id.0 == "mon"
            )
        },
    )
    .await;
    assert_eq!(
        count(&marker, 'B'),
        1,
        "the command controller must blank once"
    );

    shutdown(handle, join).await;
}

// ── (b) reload-mid-movie (F2/R2-M1, anti-tautology per P4) ──────────────────────

// Linux-only: see the audio integration note above.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::too_many_lines,
    reason = "the reload's causal observations and configuration mutation must remain together"
)]
async fn audio_playback_reload_mid_movie_refreezes_via_fresh_startup_grace() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");
    let script = write_pw_dump_script(dir.path(), AUDIO_MOVIE_FIXTURE);
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &audio_rule_config(&marker, &script, "3s", "5s", "info"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // `fake_factory` replays this SAME script from t=0 on every (re)build
    // (see its doc comment) — so each generation's own zone-absent edge
    // lands 800ms after ITS OWN start, giving that generation's poller the
    // same ordering-safety margin as case (a) above.
    let sensor_script = vec![(Duration::from_millis(800), ev("desk", SensorState::Absent))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", sensor_script),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .with_observation_hub(ObservationHub::new(64))
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut observation_rx = handle.subscribe_observations();

    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation zero entering grace",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(0),
                    rule_id: Some(rule_id),
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Active,
                    new_phase: dormant_core::state_machine::Phase::Grace { .. },
                } if rule_id.0 == "r" && display_id.0 == "mon"
            )
        },
    )
    .await;
    assert!(
        wait_for_display_inhibited(
            &handle.control_sender(),
            "mon",
            true,
            Duration::from_secs(3),
        )
        .await,
        "generation zero must be inhibited before its reload"
    );

    // Unrelated-key reload — `log_level` only; same display/rule/audio block.
    fs::write(
        &cfg_path,
        audio_rule_config(&marker, &script, "3s", "5s", "debug"),
    )
    .unwrap();
    let (request_id, receipt) = handle
        .request_reload_with_id(ReloadSource::Control)
        .await
        .expect("reload receipt");
    assert!(receipt.request_ids.contains(&request_id));
    assert_eq!(
        receipt.outcome,
        ReloadOutcome::Reloaded,
        "the unrelated log_level edit must be accepted"
    );
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation one started",
        |observation| {
            matches!(
                observation,
                DaemonObservation::GenerationStarted {
                    generation: GenerationId(1)
                }
            )
        },
    )
    .await;
    recv_observation(
        &mut observation_rx,
        Duration::from_secs(3),
        "generation one entering grace",
        |observation| {
            matches!(
                observation,
                DaemonObservation::DisplayPhaseChanged {
                    generation: GenerationId(1),
                    rule_id: Some(rule_id),
                    display_id,
                    old_phase: dormant_core::state_machine::Phase::Active,
                    new_phase: dormant_core::state_machine::Phase::Grace { .. },
                } if rule_id.0 == "r" && display_id.0 == "mon"
            )
        },
    )
    .await;
    assert!(
        wait_for_display_inhibited(
            &handle.control_sender(),
            "mon",
            true,
            Duration::from_secs(3),
        )
        .await,
        "fresh startup grace must inhibit the new generation before grace expires"
    );
    assert_eq!(count(&marker, 'B'), 0, "an inhibited grace must not blank");

    shutdown(handle, join).await;
}
