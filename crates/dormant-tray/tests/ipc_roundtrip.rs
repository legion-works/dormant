//! Integration test: spawn a real `dormantd` on an isolated socket and
//! drive `Status` / `Pause` / `Resume` through `dormantctl::client`.
//!
//! Mirrors the spawn pattern from `dormantd/tests/daemon_smoke.rs` —
//! `App::build_with_sources(...).disable_ipc()` won't do here because we
//! need the IPC server live.  So we use the public `dormantd::app::App`
//! with the daemon's own config + sensor factory plumbing and let it
//! bring up its `ipc::spawn` server.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use dormant_core::config::Strictness;
use dormant_core::config::schema::Config;
use dormant_core::fakes::FakeSensorSource;
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use dormantctl::client;
use dormantd::app::App;
use tempfile::TempDir;
use tokio::time::timeout;

fn write_file(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).expect("write file");
    path
}

fn one_display_config(socket_path: &std::path::Path) -> String {
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
blank_command = "/bin/true"
wake_command = "/bin/true"
modes = ["power_off"]

[rules.r]
zone = "office"
displays = ["mon"]
grace_period = "120ms"
min_wake_time = "0s"
wake_retries = 0
wake_retry_backoff = "10ms"
wake_retry_interval = "1s"
"#,
        sock = socket_path.display(),
    )
}

fn write_credentials(dir: &std::path::Path, toml: &str) -> PathBuf {
    let path = dir.join("credentials.toml");
    fs::write(&path, toml).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn fake_factory(
    id: &str,
) -> impl Fn(
    &Config,
    &dormant_core::config::schema::Credentials,
) -> anyhow::Result<Vec<Box<dyn SensorSource>>>
+ Send
+ Sync
+ 'static {
    // Empty script — the sensor never emits; the daemon still boots and
    // IPC comes up, which is all we need for a Status / Pause / Resume
    // round-trip.
    let template = FakeSensorSource {
        id: id.to_string(),
        script: vec![(
            Duration::from_millis(10),
            PresenceEvent::new(SensorId(id.into()), SensorState::Present, Timestamp::now()),
        )],
    };
    move |_cfg: &Config, _creds: &dormant_core::config::schema::Credentials| {
        Ok(vec![Box::new(template.clone()) as Box<dyn SensorSource>])
    }
}

async fn wait_for_socket(path: &std::path::Path) -> bool {
    for _ in 0..50 {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ipc_roundtrip_via_dormantctl_client() {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("dormant.sock");

    let cfg_path = write_file(dir.path(), "config.toml", &one_display_config(&socket_path));
    let creds_path = write_credentials(dir.path(), "ha_token = \"test\"");

    let app = App::build_with_sources(
        cfg_path,
        creds_path,
        Strictness::Strict,
        fake_factory("desk"),
    )
    .expect("build app");
    let (handle, join) = app.start().await.expect("start app");

    // Wait for the IPC socket to come up.
    assert!(
        wait_for_socket(&socket_path).await,
        "daemon socket did not appear at {}",
        socket_path.display()
    );

    // ── Status ────────────────────────────────────────────────────────────
    let status_resp = timeout(Duration::from_secs(2), async {
        tokio::task::spawn_blocking({
            let p = socket_path.clone();
            move || client::send_request(&p, &IpcRequest::Status)
        })
        .await
        .unwrap()
    })
    .await
    .expect("status timeout")
    .expect("status send_request");
    assert!(status_resp.ok, "status response: {status_resp:?}");
    let snap = status_resp
        .snapshot
        .expect("status should include snapshot");
    assert_eq!(snap.displays.len(), 1, "expected one display in snapshot");
    assert_eq!(snap.displays[0].0, "mon");

    // ── Pause ─────────────────────────────────────────────────────────────
    let pause_resp = timeout(Duration::from_secs(2), async {
        tokio::task::spawn_blocking({
            let p = socket_path.clone();
            move || {
                client::send_request(
                    &p,
                    &IpcRequest::Pause {
                        rule: None,
                        duration_s: Some(1800),
                    },
                )
            }
        })
        .await
        .unwrap()
    })
    .await
    .expect("pause timeout")
    .expect("pause send_request");
    assert!(pause_resp.ok, "pause response: {pause_resp:?}");

    // Snapshot should now show paused = true on mon.
    let status_after_pause = timeout(Duration::from_secs(2), async {
        tokio::task::spawn_blocking({
            let p = socket_path.clone();
            move || client::send_request(&p, &IpcRequest::Status)
        })
        .await
        .unwrap()
    })
    .await
    .expect("status timeout")
    .expect("status send_request");
    let snap = status_after_pause.snapshot.expect("snapshot present");
    assert!(
        snap.displays[0].1.paused,
        "mon should be paused after Pause: {:?}",
        snap.displays[0]
    );

    // ── Resume ────────────────────────────────────────────────────────────
    let resume_resp = timeout(Duration::from_secs(2), async {
        tokio::task::spawn_blocking({
            let p = socket_path.clone();
            move || client::send_request(&p, &IpcRequest::Resume { rule: None })
        })
        .await
        .unwrap()
    })
    .await
    .expect("resume timeout")
    .expect("resume send_request");
    assert!(resume_resp.ok, "resume response: {resume_resp:?}");

    // ── Shutdown ──────────────────────────────────────────────────────────
    handle.shutdown();
    let _ = timeout(Duration::from_secs(5), join).await;
}
