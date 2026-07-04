//! Real-time daemon smoke tests.
//!
//! These wire a full [`App`] over a tempdir config with `command` display
//! controllers that append marker bytes to files, plus injected
//! [`FakeSensorSource`]s (no broker/serial in CI). Timings are tight but
//! real-clock; assertions are on ordering and presence, not exact ms.

use std::fs;
use std::path::{Path, PathBuf};
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
    .expect("build app");
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
    .expect("build app");
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
    .expect("build app");
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
    .expect("build app");
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
    .expect("build app");
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
