//! Integration tests for the config-apply full loop: web UI → daemon reload
//! → filesystem artifacts.
//!
//! These boot a real [`App`] with the web UI on an ephemeral port, send HTTP
//! requests, and assert on responses, reload outcomes, and backup state.
//!
//! Port assignment uses bind-then-pass: a `TcpListener` is bound to port 0,
//! its assigned port is written into the test config, and the listener is
//! dropped before the daemon starts.  This avoids port conflicts under CI
//! parallelism while keeping the test self-contained.

#![cfg(feature = "web-ui")]

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dormant_core::config::Strictness;
use dormant_core::config::schema::Credentials;
use dormant_core::fakes::FakeSensorSource;
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use dormantd::app::App;
use reqwest::header;
use tempfile::TempDir;

// ── Helpers ────────────────────────────────────────────────────────────────────

fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).expect("write file");
    path
}

fn ev(sensor: &str, state: SensorState) -> PresenceEvent {
    PresenceEvent::new(SensorId(sensor.into()), state, Timestamp::now())
}

/// A source factory that replays `script` for `sensor` on every (re)build.
fn fake_factory(
    id: &str,
    script: Vec<(Duration, PresenceEvent)>,
) -> impl Fn(
    &dormant_core::config::schema::Config,
    &Credentials,
) -> anyhow::Result<Vec<Box<dyn SensorSource>>>
+ Send
+ Sync
+ 'static {
    let template = FakeSensorSource {
        id: id.to_string(),
        script,
    };
    move |_cfg: &dormant_core::config::schema::Config, _creds: &Credentials| {
        Ok(vec![Box::new(template.clone()) as Box<dyn SensorSource>])
    }
}

async fn shutdown(handle: dormantd::app::AppHandle, join: tokio::task::JoinHandle<()>) {
    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
}

/// Bind a TCP listener on an ephemeral port, return the port number, then
/// drop the listener so the daemon can reclaim it.
fn ephemeral_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral");
    listener.local_addr().unwrap().port()
}

/// Build a test config with web UI on `web_port`, one command-controller
/// display, one fake sensor, one zone, and one rule.
fn test_config(web_port: u16, marker: &Path, grace: &str) -> String {
    format!(
        r#"config_version = 1
[daemon]
startup_holdoff = "0s"
reload_debounce = "50ms"
web_port = {wp}
web_bind = "127.0.0.1"

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
        wp = web_port,
        m = marker.display(),
        g = grace,
    )
}

/// Build the URL for the web UI on the given port.
fn web_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// Return the Origin header value for the given port.
fn origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

// ── Test 1: full-loop happy path + backup-write-no-reload pin ─────────────────

#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_apply_full_loop() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");

    let port = ephemeral_port();
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &test_config(port, &marker, "200ms"),
    );
    let creds_path = dir.path().join("credentials.toml"); // absent → defaults

    // Room is always occupied — display will never blank, so defensive wake
    // never fires.  Keeps the apply's reload path short.
    let script = vec![(Duration::from_millis(0), ev("desk", SensorState::Present))];
    let app = App::build_with_sources(
        cfg_path.clone(),
        creds_path,
        Strictness::Strict,
        fake_factory("desk", script),
    )
    .expect("build app")
    .disable_ipc();
    let (handle, join) = app.start().await.expect("start app");

    let client = reqwest::Client::new();

    // ── GET /api/config → fingerprint ────────────────────────────────────
    let get_url = web_url(port, "/api/config");
    let cfg_resp: serde_json::Value = client
        .get(&get_url)
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .send()
        .await
        .expect("GET /api/config")
        .json()
        .await
        .expect("parse config JSON");

    let fingerprint1 = cfg_resp["fingerprint"]
        .as_str()
        .expect("fingerprint present");
    assert!(!fingerprint1.is_empty(), "fingerprint must be non-empty");
    let grace1 = cfg_resp["inventory"]["rules"]["r"]["grace_period"]
        .as_str()
        .expect("grace_period is a string");
    assert_eq!(grace1, "200ms", "initial grace_period must be 200ms");

    // ── POST /api/config/apply: set grace_period to 20s ──────────────────
    let patch_value: serde_json::Value = serde_json::Value::String("20s".into());
    let apply_body = serde_json::json!({
        "fingerprint": fingerprint1,
        "patches": [
            {
                "op": "set",
                "path": ["rules", "r", "grace_period"],
                "value": patch_value,
            }
        ]
    });

    let apply_resp: serde_json::Value = client
        .post(web_url(port, "/api/config/apply"))
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .header(header::ORIGIN, origin(port))
        .header(header::CONTENT_TYPE, "application/json")
        .json(&apply_body)
        .send()
        .await
        .expect("POST /api/config/apply")
        .json()
        .await
        .expect("parse apply JSON");

    assert_eq!(apply_resp["applied"], true, "apply must report applied");
    let reload = apply_resp["reload"].as_str().expect("reload field present");
    assert!(
        reload == "reloaded" || reload == "pending",
        "reload must be reloaded or pending, got {reload}"
    );

    // ── Follow-up GET: new fingerprint, value changed ────────────────────
    let cfg_resp2: serde_json::Value = client
        .get(&get_url)
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .send()
        .await
        .expect("GET /api/config (2)")
        .json()
        .await
        .expect("parse config JSON (2)");

    let fingerprint2 = cfg_resp2["fingerprint"]
        .as_str()
        .expect("fingerprint2 present");
    assert_ne!(
        fingerprint1, fingerprint2,
        "fingerprint must change after apply"
    );

    let grace2 = cfg_resp2["inventory"]["rules"]["r"]["grace_period"]
        .as_str()
        .expect("grace_period2 is a string");
    assert_eq!(grace2, "20s", "grace_period must now be 20s");

    // ── Backups: exactly 1 file ──────────────────────────────────────────
    let backups_dir = dir.path().join("backups");
    assert!(backups_dir.exists(), "backups dir created");
    let backup_files: Vec<_> = std::fs::read_dir(&backups_dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(backup_files.len(), 1, "exactly one backup file");
    let backup_name = backup_files[0].file_name();
    assert!(
        backup_name.to_str().unwrap().starts_with("config.toml."),
        "backup name starts with config.toml.: {backup_name:?}"
    );

    // ── Backup-write-no-reload pin ───────────────────────────────────────
    // The config watcher uses RecursiveMode::NonRecursive.  Writes to the
    // `backups/` subdirectory must not trigger a reload — the subdirectory
    // is the whole reason backups live there (spec §11.3).
    let mut reloads = handle.subscribe_reload();
    fs::write(backups_dir.join("junk"), "should be ignored").unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(1), reloads.recv()).await;
    assert!(
        outcome.is_err(), // timeout
        "backup write must NOT trigger a reload (NonRecursive watcher)"
    );

    shutdown(handle, join).await;
}

// ── Test 2: wake_command change → reloaded, not rejected ──────────────────────
//
// The apply endpoint writes the patched config, then waits for a reload outcome
// from the daemon's config watcher.  A `"rejected"` outcome can only arise from
// two paths inside `Runner::reload()`:
//
//  1. `load_and_assemble()` fails (config invalid or controllers un-buildable).
//  2. A *removed* display whose verified wake fails with the OLD executor.
//
// Path 1 is preempted by the apply handler's own `validate()` call — the
// patched config passes the same validation pipeline before the file is ever
// written, so the daemon's reload-time validation should produce the same
// result.
//
// Path 2 requires removing an entire `[displays.<id>]` entry from the config.
// The patch API only supports Set (leaf/value replacement) and Remove (leaf
// deletion of optional keys).  Entity add/remove is file-only by design
// (spec §8.3).  Neither operation can delete a top-level table from the TOML
// document, so no display is ever "removed" through the patch API.
//
// Retained-display defensive wakes (line 667 of app.rs) are fire-and-forget
// (`tokio::spawn`) — their failures are logged at WARN but never abort the
// reload.  So a patch that only changes `wake_command` to a failing value
// cannot produce a `"rejected"` outcome.
//
// This test verifies that changing `wake_command` via a patch succeeds:
// the reload completes as `"reloaded"`.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_apply_wake_command_change_reloads() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("marker");

    let port = ephemeral_port();
    let cfg_path = write_file(
        dir.path(),
        "config.toml",
        &test_config(port, &marker, "200ms"),
    );
    let creds_path = dir.path().join("credentials.toml");

    // Room absent so the display blanks (becomes "dark").  On reload, the
    // retained-dark display gets a defensive wake — but that's fire-and-
    // forget, so the reload still succeeds.
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

    // Wait for the display to blank, confirming it's in a dark phase.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let marker_content = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        marker_content.contains('B'),
        "display should have blanked before apply; marker content: {marker_content:?}"
    );

    let client = reqwest::Client::new();
    let get_url = web_url(port, "/api/config");

    // GET fingerprint.
    let cfg_resp: serde_json::Value = client
        .get(&get_url)
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .send()
        .await
        .expect("GET /api/config")
        .json()
        .await
        .expect("parse config JSON");
    let fingerprint = cfg_resp["fingerprint"].as_str().unwrap().to_string();

    // Patch wake_command to "false" (always fails).  The daemon's
    // defensive wake on reload will fail, but that failure is logged at
    // WARN and the reload proceeds to Reloaded.
    let apply_body = serde_json::json!({
        "fingerprint": fingerprint,
        "patches": [
            {
                "op": "set",
                "path": ["displays", "mon", "wake_command"],
                "value": "false",
            }
        ]
    });

    let apply_resp: serde_json::Value = client
        .post(web_url(port, "/api/config/apply"))
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .header(header::ORIGIN, origin(port))
        .header(header::CONTENT_TYPE, "application/json")
        .json(&apply_body)
        .send()
        .await
        .expect("POST /api/config/apply")
        .json()
        .await
        .expect("parse apply JSON");

    assert_eq!(apply_resp["applied"], true);
    let reload = apply_resp["reload"].as_str().unwrap();
    // The reload must be "reloaded" — setting a failing wake_command does
    // not trigger a Rejected outcome because defensive wake failures are
    // fire-and-forget (see test module docs).
    assert_eq!(
        reload, "reloaded",
        "wake_command change must reload successfully (defensive wake failure is fire-and-forget); got {reload}"
    );

    shutdown(handle, join).await;
}
