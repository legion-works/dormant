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
use dormant_core::rules::{ControlMsg, DaemonEvent, StateSnapshot};
use dormant_core::traits::SensorSource;
use dormant_core::types::{DisplayId, PresenceEvent, SensorId, SensorState, Timestamp};
use dormantd::app::{App, ReloadOutcome, validate_only};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tracing_subscriber::fmt::MakeWriter;

// ── Helpers ────────────────────────────────────────────────────────────────────

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
async fn reload_swap() {
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
async fn removed_display_verified_wake() {
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
async fn removed_display_wake_failure_aborts() {
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
async fn reload_defensive_wake_retained() {
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
    fs::write(&cfg_path, one_display_config(&marker, "150ms")).unwrap();
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
async fn ruleless_display_preserves_phase_on_reload() {
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

    let mut config_watch = handle.config_watch();
    let initial_holdoff = config_watch.borrow().daemon.startup_holdoff;
    assert_eq!(initial_holdoff, Duration::from_secs(0));

    // Wait for the initial blank so the run loop has fully settled.
    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur"
    );

    // Write a valid config with a different startup_holdoff and reload.
    let modified = one_display_config(&marker, "400ms")
        .replace("startup_holdoff = \"0s\"", "startup_holdoff = \"5s\"");
    fs::write(&cfg_path, &modified).unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    // The watch should deliver the updated config.
    let changed = tokio::time::timeout(Duration::from_secs(2), config_watch.changed())
        .await
        .expect("watch changed in time");
    assert!(
        changed.is_ok(),
        "config watch must reflect successful reload"
    );
    assert_eq!(
        config_watch.borrow().daemon.startup_holdoff,
        Duration::from_secs(5)
    );

    // Now trigger a rejected reload — the watch must NOT change.
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

    // The watch should NOT have changed after a rejected reload.
    // Because watch::changed() would hang if no update is sent, we use
    // a short timeout to confirm no update arrives.
    let no_change = tokio::time::timeout(Duration::from_millis(300), config_watch.changed()).await;
    assert!(
        no_change.is_err(),
        "config watch must NOT update after a rejected reload"
    );

    shutdown(handle, join).await;
}

// ── 9: creds_watch updates on successful reload only ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creds_watch_updates_on_successful_reload_only() {
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
    assert_eq!(creds_watch.borrow().ha_token, Some("initial".to_string()));

    // Wait for the initial blank so the run loop has fully settled.
    assert!(
        wait_for(|| count(&marker, 'B') >= 1, Duration::from_secs(3)).await,
        "first blank should occur"
    );

    // Update the credentials and reload — watch must see the new value.
    let _creds_path = write_credentials(dir.path(), "ha_token = \"updated\"");
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome in time")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);

    let changed = tokio::time::timeout(Duration::from_secs(2), creds_watch.changed())
        .await
        .expect("creds watch changed in time");
    assert!(
        changed.is_ok(),
        "creds watch must reflect successful reload"
    );
    assert_eq!(creds_watch.borrow().ha_token, Some("updated".to_string()));

    // Trigger a rejected reload — creds watch must NOT change.
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

    let no_change = tokio::time::timeout(Duration::from_millis(300), creds_watch.changed()).await;
    assert!(
        no_change.is_err(),
        "creds watch must NOT update after a rejected reload"
    );

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
async fn web_bind_change_ignored_on_reload() {
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

#[cfg(feature = "render")]
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
    ) -> Option<Arc<dyn dormant_core::traits::RenderSink>>
    + Send
    + Sync
    + 'static {
        move |_did, _output, _tx, _ss| Some(Arc::new(sink.clone()))
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
    /// **Determinism — identity-based, no counts.** The test factory pairs
    /// each render sink it builds with the `InputWake` sender it was handed,
    /// in build order.  Selection never counts invocations (that total is
    /// environment-dependent — a single `fs::write` can wake both
    /// `trigger_reload` and the config watcher, and CPU load shifts timing).
    /// Instead, after draining the reload bus until quiet (so the installed
    /// generation is FINAL) and re-driving the engine to a render stage, the
    /// test selects the sink pinned by three identity marks: index >= 2 (only
    /// a rollback generation lands here — indices 0..2 are the initial
    /// assembly, and a rejected new-assembly's sink is dropped before its
    /// generation is ever spawned, so it never renders), a LIVE sender (only
    /// the CURRENT generation's `InputWake` receiver is still held by a drain
    /// — superseded rollbacks are closed), and an actual `RenderBlack` (so a
    /// teardown is observable).  Among matches it takes the highest index
    /// (build order → newest generation).  No control-channel bypass, no
    /// count gate.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollback_input_wake_routes_through_drain() {
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
        let builder =
            move |_did: dormant_core::types::DisplayId,
                  _output: String,
                  tx: Option<
                &tokio::sync::mpsc::UnboundedSender<dormant_core::types::DisplayId>,
            >,
                  _ss: Option<&dormant_render::ScreensaverSettings>| {
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
        let mut reloads = handle.subscribe_reload();

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
        assert!(handle.trigger_reload().await);

        let outcome = tokio::time::timeout(Duration::from_secs(5), reloads.recv())
            .await
            .expect("reload outcome")
            .expect("reload bus open");
        match outcome {
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

        // A single `fs::write` wakes BOTH the explicit `trigger_reload` AND
        // the config file watcher, and `run_loop` services reload triggers
        // serially — so a SECOND (watcher-driven) reload can follow the
        // first, replacing its rollback generation (a `tokio::select!` race).
        // Drain the reload bus until it goes quiet for longer than
        // `reload_debounce` (500ms default): once no further outcome arrives
        // and the test issues no more config writes, no new reload cycle can
        // start, so the generation now installed is FINAL.
        while (tokio::time::timeout(Duration::from_millis(900), reloads.recv()).await).is_ok() {
            // Keep draining until the bus is quiet.
        }

        // Drive the final restored generation back to a render stage so its
        // rollback sink renders and a teardown becomes observable.
        let events = handle.events_sender();
        let _ = events.send(ev("desk", SensorState::Present)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = events.send(ev("desk", SensorState::Absent)).await;

        // Identity selection (NO counts): find the render sink of the CURRENT
        // (final) generation and use ITS own channel.  Three identity marks
        // pin it exactly:
        //   * index >= 2 — indices 0..2 are the initial assembly; a rejected
        //     new-assembly's sink is dropped before its generation is ever
        //     spawned, so only a rollback generation's sink lands here;
        //   * a LIVE sender (`!is_closed()`) — a generation's `InputWake`
        //     receiver is held by its spawned drain and dropped when the
        //     generation is torn down, so only the CURRENT generation's sink
        //     still has an open sender (superseded rollbacks are closed);
        //   * that has rendered `RenderBlack` — so a teardown is observable.
        // Take the highest-index match (build order → the newest generation).
        let live_pair = |sinks: &[WakePair]| -> Option<WakePair> {
            sinks
                .iter()
                .enumerate()
                .rfind(|(i, (sink, tx))| *i >= 2 && !tx.is_closed() && sink_rendered(sink))
                .map(|(_, pair)| pair.clone())
        };
        let selected = wait_for(
            || live_pair(&pairs.lock().unwrap()[..]).is_some(),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            selected,
            "no live post-initial render sink ever rendered — rebuild_old did not \
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
async fn manual_only_display_no_defensive_wake_on_reload() {
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
async fn rule_driven_dark_display_defensive_woken_on_reload() {
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
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", vec![]),
    )
    .expect("build app")
    .with_notify_sink_builder(noop_factory)
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");
    let mut reloads = handle.subscribe_reload();
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

    // Verify phase is "blanked" in the snapshot.
    {
        let (tx, rx) = oneshot::channel();
        ctl.send(ControlMsg::Snapshot(tx)).await.unwrap();
        let snap: StateSnapshot = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("snapshot in time")
            .expect("snapshot reply");
        let manual_snap = snap
            .displays
            .iter()
            .find(|(id, _)| id == "manual")
            .map(|(_, ds)| ds)
            .unwrap();
        assert_eq!(
            manual_snap.phase, "blanked",
            "phase must be blanked after ForceBlank, got {:?}",
            manual_snap.phase
        );
    }

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
    // Drain reload outcomes until the bus is quiet, then assert the last
    // one was Reloaded.  The watcher debounce is 500ms; a 1s settle window
    // is tight but sufficient for a single deterministic reload.
    let settle = Duration::from_secs(1);
    let mut last_outcome = None;
    while let Ok(Ok(o)) = tokio::time::timeout(settle, reloads.recv()).await {
        last_outcome = Some(o);
    }
    assert_eq!(
        last_outcome,
        Some(ReloadOutcome::Reloaded),
        "reload must settle as Reloaded"
    );

    // ── 4. Assert across reload: NO wake, phase preserved ──────────────
    let manual_w_after = count(&manual_marker, 'W');
    assert_eq!(
        manual_w_after,
        manual_wake_before,
        "manual-only display must NOT be defensive-woken on reload, \
         got {manual_w_after} W (was {manual_wake_before} W before), marker={:?}",
        read(&manual_marker)
    );

    {
        // Retry across the reload generation-switch window (issue #9).
        let snap = snapshot_with_retry(&handle.control_sender()).await;
        let manual_snap = snap
            .displays
            .iter()
            .find(|(id, _)| id == "manual")
            .map(|(_, ds)| ds)
            .unwrap();
        assert_eq!(
            manual_snap.phase, "blanked",
            "manual-only display must preserve blanked phase after reload, got {:?}",
            manual_snap.phase
        );
    }

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

    // Poll until the snapshot reflects the phase transition (the wake
    // command writes the marker before the engine processes WakeResult).
    let mut final_phase = String::new();
    for _ in 0..100 {
        // ~5s bounded
        let (tx, rx) = oneshot::channel();
        if ctl.send(ControlMsg::Snapshot(tx)).await.is_ok()
            && let Ok(Ok(snap)) = tokio::time::timeout(Duration::from_millis(200), rx).await
            && let Some((_, d)) = snap.displays.iter().find(|(id, _)| id == "manual")
        {
            final_phase = d.phase.clone();
            if final_phase == "active" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        final_phase, "active",
        "manual-only display must be active after ForceWake, got {final_phase:?}"
    );

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

    // The tracker's first tick fires immediately regardless of
    // sample_interval — a disabled tracker parks on that very first tick,
    // so a short real-time wait is enough to prove no file ever appears.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let wear_dir = state_home.path().join("dormant").join("wear");
    let entries = fs::read_dir(&wear_dir).map_or(0, Iterator::count);

    shutdown(handle, join).await;

    assert_eq!(
        entries, 0,
        "wear.enabled = false must create no files under the wear state dir"
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
/// **Post-shutdown poll, not an immediate read**: `App::start`'s wear-
/// tracker `JoinHandle` is intentionally fire-and-forget (never joined —
/// dropping a `JoinHandle` doesn't abort the task; see `app.rs`'s wiring
/// notes). `shutdown()` only awaits the daemon's main `run_loop` task, so
/// it can return before the SEPARATE wear-tracker task has finished
/// executing its own cancellation-triggered `persist_all_dirty` — reading
/// the file immediately after `shutdown()` races that fire-and-forget
/// completion (confirmed via a temporary `tracing`-capture diagnostic run:
/// the in-memory ledger's `sample_count` climbed correctly tick-by-tick,
/// but the file still showed the stale pre-shutdown value on the losing
/// side of the race). A short bounded poll after `shutdown()` accommodates
/// this without weakening what the test actually proves.
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

    shutdown(handle, join).await;

    // Bounded poll (see doc comment above) for the wear tracker's own
    // fire-and-forget cancellation-triggered flush to land on disk. Widened
    // from 3s to 8s (G1 review fix) — this only costs time on the losing
    // side (predicate already true returns immediately), so it's a free
    // safety margin for a loaded CI runner.
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
        serde_json::from_str(&contents).expect("ledger file must still parse after shutdown");

    assert!(
        ok && ledger.sample_count > sample_count_at_first_persist,
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
async fn reload_carries_last_blank_failed_until_dispatch_relevant_edit() {
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
async fn notifier_closes_stale_episode_from_new_generation_startup_reconcile() {
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
async fn reload_carries_sensor_reported_until_own_config_edit() {
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
