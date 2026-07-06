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
use dormant_core::rules::{ControlMsg, StateSnapshot};
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use dormantd::app::{App, ReloadOutcome, validate_only};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tracing_subscriber::fmt::MakeWriter;

// ── Helpers ────────────────────────────────────────────────────────────────────

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
    .expect("build app");
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

// ── Display dropped from rules but kept in [displays] is treated as removed ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ruleless_display_verified_wake() {
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

    // Keep mon2 in [displays] but drop it from every rule — it becomes inert
    // (no executor in the new generation), so it must be treated as removed
    // and woken via its OLD executor before the reload succeeds.
    fs::write(&cfg_path, two_display_config(&m1, &m2, &wake2, true, false)).unwrap();
    assert!(handle.trigger_reload().await);

    let outcome = tokio::time::timeout(Duration::from_secs(3), reloads.recv())
        .await
        .expect("reload outcome")
        .expect("reload bus open");
    assert_eq!(outcome, ReloadOutcome::Reloaded);
    assert!(
        wait_for(|| count(&m2, 'W') >= 1, Duration::from_secs(3)).await,
        "rule-less dropped display must get a verified wake via its old executor, mon2={:?}",
        read(&m2)
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
    fn recording_factory(
        sink: RecordingRenderSink,
    ) -> impl Fn(
        dormant_core::types::DisplayId,
        String,
    ) -> Option<Arc<dyn dormant_core::traits::RenderSink>>
    + Send
    + Sync
    + 'static {
        move |_did, _output| Some(Arc::new(sink.clone()))
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
}
