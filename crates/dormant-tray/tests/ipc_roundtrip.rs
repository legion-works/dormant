//! Integration test: spawn a real `dormantd` on an isolated socket and
//! drive `Status` / `Pause` / `Resume` through `dormantctl::client`.
//!
//! Mirrors the spawn pattern from `dormantd/tests/daemon_smoke.rs` —
//! `App::build_with_sources(...).disable_ipc()` won't do here because we
//! need the IPC server live.  So we use the public `dormantd::app::App`
//! with the daemon's own config + sensor factory plumbing and let it
//! bring up its `ipc::spawn` server.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use dormant_core::config::Strictness;
use dormant_core::config::schema::Config;
use dormant_core::fakes::FakeSensorSource;
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::rules::DaemonEvent;
use dormant_core::traits::SensorSource;
use dormant_core::types::{PresenceEvent, SensorId, SensorState, Timestamp};
use dormantctl::client;
use dormantd::app::App;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

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

fn noop_factory() -> std::sync::Arc<dyn NotifySink> {
    std::sync::Arc::new(NoopNotifySink)
}

/// Drain the event stream until a `DisplayPhase` frame arrives, skipping any
/// non-target frames the daemon legitimately emits first.
///
/// Only event ARRIVAL is guaranteed — the daemon may publish a
/// `WearSnapshot` (the wear tracker's first tick fires at startup and emits
/// one because each display's persist clock starts at zero), a
/// `CompensationAdvisory`, or `SensorChanged`/`ZoneChanged` (the fake
/// sensor's startup sample) before the blank's `DisplayPhase`, and the
/// ordering shifts under load. Asserting the FIRST frame is `DisplayPhase`
/// is an event-ORDER assumption the daemon does not guarantee. This drain
/// mirrors `recv_terminal_outcome` in `daemon_smoke.rs` (bounded by a
/// deadline, no sleeps) and the `spawn_event_pump` reader in `ipc_loop.rs`:
/// an honest timeout panic lists every frame observed so the failure stays
/// diagnosable.
async fn assert_event_streams_a_daemon_event(
    events: client::EventStream,
    event_shutdown: client::EventShutdown,
) {
    let (tx, mut rx) = mpsc::channel::<Result<DaemonEvent, anyhow::Error>>(32);
    let reader = tokio::task::spawn_blocking(move || {
        let mut events = events;
        for result in events.by_ref() {
            if tx.blocking_send(result).is_err() {
                break; // driver dropped the receiver — stop reading
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed: Vec<String> = Vec::new();
    let mut got: Option<DaemonEvent> = None;
    let mut stopped: Option<&'static str> = None;
    while got.is_none() && stopped.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stopped = Some("deadline hit while waiting for DisplayPhase");
            break;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(Ok(event))) => {
                if matches!(event, DaemonEvent::DisplayPhase { .. }) {
                    got = Some(event);
                } else {
                    observed.push(format!("{event:?}"));
                }
            }
            Ok(Some(Err(e))) => observed.push(format!("decode error: {e}")),
            Ok(None) => stopped = Some("event stream closed before DisplayPhase"),
            Err(_) => stopped = Some("deadline hit while waiting for DisplayPhase"),
        }
    }

    // Release the reader: drop the receiver so a parked `blocking_send`
    // returns, and shutdown the socket so a parked `read_line` returns EOF.
    drop(rx);
    let _ = event_shutdown.shutdown();
    let _ = timeout(Duration::from_secs(2), reader).await;

    match got {
        Some(event) => assert!(
            matches!(event, DaemonEvent::DisplayPhase { .. }),
            "internal: drain returned a non-DisplayPhase event: {event:?}"
        ),
        None => panic!(
            "{}; observed: {observed:?}",
            stopped.unwrap_or("stopped waiting for DisplayPhase")
        ),
    }
}

async fn force_blank(socket_path: &std::path::Path) {
    let p = socket_path.to_path_buf();
    let blank_resp = timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            client::send_request(
                &p,
                &IpcRequest::Blank {
                    display: "mon".to_string(),
                },
            )
        }),
    )
    .await
    .expect("blank timeout")
    .expect("blank reader task")
    .expect("blank send_request");
    assert!(blank_resp.ok, "blank response: {blank_resp:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ipc_status_and_events_roundtrip_via_dormantctl_client() {
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
    .expect("build app")
    .with_notify_sink_builder(noop_factory);
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

    // ── Force blank + events ──────────────────────────────────────────────
    let (events, event_shutdown) = client::connect_events(&socket_path).expect("connect events");
    force_blank(&socket_path).await;

    assert_event_streams_a_daemon_event(events, event_shutdown).await;

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

    assert!(
        !zbus_sink_was_constructed(),
        "ZbusSink must never be constructed — every App construction site must inject a no-op notify sink"
    );
}
