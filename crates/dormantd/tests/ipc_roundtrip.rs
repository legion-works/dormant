//! Integration tests for the IPC server: spawn a real `IpcServer` on a temp
//! socket with a fake control loop, then connect as a client and verify
//! request/response round-trips.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::rules::{
    ControlMsg, DaemonEvent, DisplaySnapshot, SensorSnapshot, StateSnapshot, ZoneSnapshot,
};
use dormant_core::types::{RuleId, SensorState};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Spawn a fake engine control loop that responds to Snapshot with a canned
/// state and records all other `ControlMsg`s.
fn spawn_fake_engine() -> (
    mpsc::Sender<ControlMsg>,
    broadcast::Sender<DaemonEvent>,
    mpsc::Receiver<ControlMsg>,
) {
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(64);
    let (event_tx, _) = broadcast::channel(256);
    let (record_tx, record_rx) = mpsc::channel::<ControlMsg>(64);

    let canned_snapshot = StateSnapshot {
        sensors: vec![SensorSnapshot {
            id: "desk".into(),
            state: SensorState::Present,
            last_seen_secs_ago: 2,
        }],
        zones: vec![ZoneSnapshot {
            id: "office".into(),
            present: Some(true),
        }],
        displays: vec![
            (
                "main_monitor".into(),
                DisplaySnapshot {
                    phase: "active".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 1,
                },
            ),
            (
                "tv".into(),
                DisplaySnapshot {
                    phase: "blanked".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 3,
                },
            ),
        ],
        pending_reload: None,
    };

    let event_tx_for_spawn = event_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            match msg {
                ControlMsg::Snapshot(tx) => {
                    let _ = tx.send(canned_snapshot.clone());
                }
                ControlMsg::SubscribeEvents(tx) => {
                    let _ = tx.send(event_tx_for_spawn.subscribe());
                }
                other => {
                    let _ = record_tx.send(other).await;
                }
            }
        }
    });

    (ctl_tx, event_tx, record_rx)
}

/// Connect to a Unix socket, send a JSON request, read one response line.
async fn send_request(socket_path: &Path, request: &IpcRequest) -> IpcResponse {
    let stream = UnixStream::connect(socket_path).await.unwrap();
    let (reader, mut writer) = tokio::io::split(stream);
    let line = serde_json::to_string(request).unwrap();
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let mut reader = BufReader::new(reader);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await.unwrap();

    serde_json::from_str(response_line.trim()).unwrap()
}

/// Create a tempdir with a socket path and spawn the IPC server.
/// Returns `(dir, socket_path, ctl_tx, event_tx, record_rx, cancel)`.
async fn setup_server() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    mpsc::Sender<ControlMsg>,
    broadcast::Sender<DaemonEvent>,
    mpsc::Receiver<ControlMsg>,
    CancellationToken,
) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("dormant.sock");

    let (ctl_tx, event_tx, record_rx) = spawn_fake_engine();
    let (reload_tx, _reload_rx) = mpsc::channel::<()>(8);
    let cancel = CancellationToken::new();

    let _handle =
        dormantd::ipc::spawn(&socket_path, ctl_tx.clone(), reload_tx, cancel.clone()).unwrap();

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;

    (dir, socket_path, ctl_tx, event_tx, record_rx, cancel)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_roundtrip_parses_snapshot() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    let resp = send_request(&socket_path, &IpcRequest::Status).await;

    assert!(resp.ok);
    let snap = resp.snapshot.expect("status should return a snapshot");
    assert_eq!(snap.sensors.len(), 1);
    assert_eq!(snap.sensors[0].id, "desk");
    assert_eq!(snap.displays.len(), 2);
    assert!(snap.displays.iter().any(|(id, _)| id == "main_monitor"));

    cancel.cancel();
}

#[tokio::test]
async fn pause_sends_control_msg() {
    let (_dir, socket_path, _ctl_tx, _event_tx, mut record_rx, cancel) = setup_server().await;

    let resp = send_request(
        &socket_path,
        &IpcRequest::Pause {
            rule: Some("office".into()),
            duration_s: Some(7200),
        },
    )
    .await;

    assert!(resp.ok);

    // Check the recorded control message
    let recorded = tokio::time::timeout(Duration::from_secs(1), record_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match recorded {
        ControlMsg::Pause { rule, until } => {
            assert_eq!(rule, Some(RuleId("office".into())));
            assert!(until.is_some());
            // until should be ~now + 7200s
            let now = std::time::SystemTime::now();
            let until_time = until.unwrap().0;
            let diff = until_time.duration_since(now).unwrap_or(Duration::ZERO);
            // Allow 2s of test jitter
            assert!(
                diff.as_secs() > 7198 && diff.as_secs() < 7300,
                "until should be ~7200s from now, got {diff:?}"
            );
        }
        other => panic!("expected Pause, got {other:?}"),
    }

    cancel.cancel();
}

#[tokio::test]
async fn blank_unknown_display_returns_error() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    let resp = send_request(
        &socket_path,
        &IpcRequest::Blank {
            display: "nonexistent".into(),
        },
    )
    .await;

    assert!(!resp.ok);
    let err = resp.error.expect("should have error");
    assert!(
        err.contains("nonexistent"),
        "error should mention display name: {err}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn events_streams_two_events_then_disconnect() {
    let (_dir, socket_path, _ctl_tx, event_tx, _record_rx, cancel) = setup_server().await;

    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (reader, mut writer) = tokio::io::split(stream);
    let request = IpcRequest::Events;
    let line = serde_json::to_string(&request).unwrap();
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    // Give the server a moment to subscribe.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send two events through the broadcast channel.
    let ev1 = DaemonEvent::ConfigReloaded;
    let ev2 = DaemonEvent::SensorChanged {
        sensor: dormant_core::types::SensorId("desk".into()),
        state: dormant_core::types::SensorState::Present,
    };
    assert!(event_tx.send(ev1).is_ok());
    assert!(event_tx.send(ev2).is_ok());

    // Read both events from the stream with a timeout.
    let mut reader = BufReader::new(reader);
    let mut line1 = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line1))
        .await
        .expect("timeout reading event1")
        .unwrap();
    let event1: DaemonEvent = serde_json::from_str(line1.trim()).unwrap();
    assert!(matches!(event1, DaemonEvent::ConfigReloaded));

    let mut line2 = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line2))
        .await
        .expect("timeout reading event2")
        .unwrap();
    let event2: DaemonEvent = serde_json::from_str(line2.trim()).unwrap();
    match event2 {
        DaemonEvent::SensorChanged { sensor, state } => {
            assert_eq!(sensor.0, "desk");
            assert_eq!(state, dormant_core::types::SensorState::Present);
        }
        _ => panic!("expected SensorChanged"),
    }

    cancel.cancel();
}

#[tokio::test]
async fn bad_json_line_returns_error_and_connection_stays_usable() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    // Use a fresh connection for each request.
    let resp = send_request(&socket_path, &IpcRequest::Status).await;
    assert!(resp.ok, "baseline status should work");

    // Send bad JSON via raw write
    {
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        writer.write_all(b"not valid json\n").await.unwrap();
        writer.flush().await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut response_line))
            .await
            .expect("timeout reading bad-json response")
            .unwrap();
        let resp: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        assert!(!resp.ok);
        assert!(
            resp.error.as_deref().unwrap().contains("bad request"),
            "error should mention bad request: {:?}",
            resp.error
        );
    }

    // Verify a subsequent connection still works
    let resp2 = send_request(&socket_path, &IpcRequest::Status).await;
    assert!(resp2.ok, "connection should still be usable after bad JSON");

    cancel.cancel();
}

#[tokio::test]
async fn socket_file_permissions_0600() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("dormant.sock");

    let (ctl_tx, _event_tx, _record_rx) = spawn_fake_engine();
    let (reload_tx, _reload_rx) = mpsc::channel::<()>(8);
    let cancel = CancellationToken::new();

    let _handle = dormantd::ipc::spawn(&socket_path, ctl_tx, reload_tx, cancel.clone()).unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let metadata = std::fs::metadata(&socket_path).unwrap();
    let mode = metadata.permissions().mode();
    // Only check the permission bits (0o600 = owner read+write)
    assert_eq!(
        mode & 0o777,
        0o600,
        "socket permissions should be 0600, got {mode:o}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn stale_socket_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("dormant.sock");

    // Create a stale socket file (dead daemon)
    std::fs::write(&socket_path, "stale").unwrap();

    let (ctl_tx, _event_tx, _record_rx) = spawn_fake_engine();
    let (reload_tx, _reload_rx) = mpsc::channel::<()>(8);
    let cancel = CancellationToken::new();

    // Should succeed — replaces the stale socket
    let result = dormantd::ipc::spawn(&socket_path, ctl_tx, reload_tx, cancel.clone());
    assert!(result.is_ok(), "should replace stale socket: {result:?}");

    // Verify the socket file exists and is connectable
    assert!(socket_path.exists(), "socket file should exist");
    let _ = std::os::unix::net::UnixStream::connect(&socket_path)
        .expect("should be able to connect to socket");

    cancel.cancel();
}
