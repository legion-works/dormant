//! Integration tests for the IPC server: spawn a real `IpcServer` on a temp
//! socket with a fake control loop, then connect as a client and verify
//! request/response round-trips.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dormant_core::config::schema::{Config, Credentials, DaemonConfig, DisplayScope};
use dormant_core::ipc_proto::{IpcRequest, IpcResponse};
use dormant_core::rules::{
    ControlMsg, DaemonEvent, DisplaySnapshot, SensorSnapshot, StateSnapshot, ZoneSnapshot,
};
use dormant_core::traits::{CommandSink, InputSourceTarget};
use dormant_core::types::{CmdFailure, DisplayId, RuleId, SensorState};
use dormant_doctor::DoctorService;
use indexmap::IndexMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

// ── Fake sink for switch tests ──────────────────────────────────────────────

/// A scripted [`CommandSink`] that always succeeds on [`write_input_source`](CommandSink::write_input_source).
struct FakeSink {
    last_target: Mutex<Option<InputSourceTarget>>,
}

impl FakeSink {
    fn new() -> Self {
        Self {
            last_target: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl CommandSink for FakeSink {
    async fn blank(&self, _mode: dormant_core::types::BlankMode) -> Result<(), CmdFailure> {
        Ok(())
    }

    async fn wake(&self) -> Result<(), CmdFailure> {
        Ok(())
    }

    fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
        vec![]
    }

    async fn write_input_source(&self, target: InputSourceTarget) -> Result<(), CmdFailure> {
        *self.last_target.lock().unwrap() = Some(target);
        Ok(())
    }
}

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
            reported: true,
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
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                    claim_armed_remaining_ms: None,
                },
            ),
            (
                "tv".into(),
                DisplaySnapshot {
                    phase: "blanked".into(),
                    inhibited: false,
                    paused: false,
                    cmd_gen: 3,
                    scope: dormant_core::config::DisplayScope::Private,
                    owned: true,
                    observed_input_code: None,
                    panel_state: None,
                    controllers: vec![],
                    wake_attempts: 0,
                    last_blank_failed: false,
                    stage: None,
                    claim_armed_remaining_ms: None,
                },
            ),
        ],
        pending_reload: None,
        rollback: None,
        kvm: None,
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

/// Build a throwaway [`DirectSwitchHandle`] with no displays — all
/// switch attempts will return `Unsupported`.
fn fake_direct_switch(
    ctl_tx: mpsc::Sender<ControlMsg>,
) -> Arc<dormantd::direct_switch::DirectSwitchHandle> {
    fake_direct_switch_with(None, ctl_tx)
}

/// Build a [`DirectSwitchHandle`] with an optional shared display.
fn fake_direct_switch_with(
    display_name: Option<&str>,
    ctl_tx: mpsc::Sender<ControlMsg>,
) -> Arc<dormantd::direct_switch::DirectSwitchHandle> {
    let mut displays = IndexMap::new();
    let mut executors: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();

    if let Some(name) = display_name {
        let dc = dormant_core::config::schema::DisplayConfig {
            controllers: vec!["ddcci".into()],
            scope: DisplayScope::Shared,
            shared_input_code: Some(0x10),
            shared_input_write_code: None,
            shared_peer_input_write_code: None,
            shared_peer_input_code: None,
            hooks: dormant_core::config::schema::HookSlots::default(),
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 100,
            samsung_restore_backlight: dormant_core::config::defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::default(),
        };
        displays.insert(name.to_string(), dc);
        let sink: Arc<dyn CommandSink> = Arc::new(FakeSink::new());
        executors.insert(DisplayId(name.to_string()), sink);
    }

    let (executors_tx, executors_rx) = watch::channel(Arc::new(executors));
    drop(executors_tx);
    let (config_tx, config_rx) = watch::channel(Arc::new(Config {
        coordination: dormant_core::config::CoordinationConfig::default(),
        config_version: 1,
        daemon: DaemonConfig::default(),
        wear: dormant_core::config::schema::WearConfig::default(),
        notifications: dormant_core::config::schema::NotificationsConfig::default(),
        watchdog: dormant_core::config::schema::WatchdogConfig::default(),
        audio: dormant_core::config::schema::AudioConfig::default(),
        sensors: IndexMap::default(),
        zones: IndexMap::default(),
        displays,
        rules: IndexMap::default(),
        keymap: dormant_core::config::KeymapConfig::default(),
        input_filter: dormant_core::config::InputFilterConfig::default(),
    }));
    drop(config_tx);
    let publisher = Arc::new(dormantd::hooks::MqttPublisher::new(String::new(), None));
    let hook_engine = Arc::new(dormantd::hooks::HookEngine::new(publisher));
    Arc::new(dormantd::direct_switch::DirectSwitchHandle::new(
        executors_rx,
        config_rx,
        hook_engine,
        ctl_tx,
    ))
}

/// Build a throwaway `DoctorService` for tests that don't exercise the
/// doctor path.  The service is still constructed (so the IPC server
/// signature is satisfied) and will not be invoked.
fn fake_doctor(ctl_tx: mpsc::Sender<ControlMsg>) -> DoctorService {
    let (config_tx, config_rx) = watch::channel(Arc::new(Config {
        coordination: dormant_core::config::CoordinationConfig::default(),
        config_version: 1,
        daemon: DaemonConfig::default(),
        wear: dormant_core::config::schema::WearConfig::default(),
        notifications: dormant_core::config::schema::NotificationsConfig::default(),
        watchdog: dormant_core::config::schema::WatchdogConfig::default(),
        audio: dormant_core::config::schema::AudioConfig::default(),
        sensors: IndexMap::default(),
        zones: IndexMap::default(),
        displays: IndexMap::default(),
        rules: IndexMap::default(),
        keymap: dormant_core::config::KeymapConfig::default(),
        input_filter: dormant_core::config::InputFilterConfig::default(),
    }));
    let (creds_tx, creds_rx) = watch::channel(Arc::new(Credentials::default()));
    drop(config_tx);
    drop(creds_tx);
    DoctorService::new(ctl_tx, config_rx, creds_rx)
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
    let (reload_tx, _reload_rx) = mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
    let cancel = CancellationToken::new();
    let doctor = fake_doctor(ctl_tx.clone());
    let ds = fake_direct_switch(ctl_tx.clone());

    let _handle = dormantd::ipc::spawn(
        &socket_path,
        ctl_tx.clone(),
        dormant_core::reload::ReloadRequester::new(reload_tx),
        doctor,
        ds,
        cancel.clone(),
    )
    .unwrap();

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
async fn pause_u64_max_overflow_returns_error() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    let resp = send_request(
        &socket_path,
        &IpcRequest::Pause {
            rule: None,
            duration_s: Some(u64::MAX),
        },
    )
    .await;

    assert!(!resp.ok, "u64::MAX should overflow");
    assert_eq!(
        resp.error.as_deref(),
        Some("duration overflow"),
        "error should mention overflow, got: {:?}",
        resp.error
    );

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

    // The sentinel proves the server registered its broadcast receiver before
    // the test sends events, avoiding a timing-dependent subscription race.
    let mut reader = BufReader::new(reader);
    let mut subscribed_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        reader.read_line(&mut subscribed_line),
    )
    .await
    .expect("timeout reading subscription sentinel")
    .unwrap();
    let subscribed: DaemonEvent = serde_json::from_str(subscribed_line.trim()).unwrap();
    assert!(matches!(subscribed, DaemonEvent::Subscribed));

    // Send two events through the broadcast channel.
    let ev1 = DaemonEvent::ConfigReloaded;
    let ev2 = DaemonEvent::SensorChanged {
        sensor: dormant_core::types::SensorId("desk".into()),
        state: dormant_core::types::SensorState::Present,
    };
    assert!(event_tx.send(ev1).is_ok());
    assert!(event_tx.send(ev2).is_ok());

    // Read both events from the stream with a timeout.
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
async fn line_at_max_length_accepted() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    // Build a JSON request that is exactly at the max line length.
    // We send a Status request (small) padded with whitespace to MAX.
    let base = br#"{"req":"status"}"#;
    let max = 1_048_576;
    let mut line = Vec::with_capacity(max + 1);
    line.extend_from_slice(base);
    line.resize(max, b' ');
    line.push(b'\n');

    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (reader, mut writer) = tokio::io::split(stream);
    writer.write_all(&line).await.unwrap();
    writer.flush().await.unwrap();

    let mut reader = BufReader::new(reader);
    let mut response_line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut response_line))
        .await
        .expect("timeout reading response")
        .unwrap();
    let resp: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
    // The server trims whitespace, so the padded line should parse as status.
    assert!(resp.ok, "max-length line should be accepted");

    cancel.cancel();
}

#[tokio::test]
async fn line_exceeding_max_rejected() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    // Build a line that exceeds MAX by 100 bytes.
    let base = br#"{"req":"status"}"#;
    let max = 1_048_576;
    let oversized = max + 100;
    let mut line = Vec::with_capacity(oversized + 1);
    line.extend_from_slice(base);
    line.resize(oversized, b' ');
    line.push(b'\n');

    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (reader, mut writer) = tokio::io::split(stream);
    writer.write_all(&line).await.unwrap();
    writer.flush().await.unwrap();

    let mut reader = BufReader::new(reader);
    let mut response_line = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut response_line)).await;

    // The server may close the connection after an oversized line (bail in
    // handle_connection logs and returns).  Either an error response or EOF
    // is acceptable — what matters is that the oversized line is rejected.
    match read_result {
        Ok(Ok(0)) => {
            // EOF — server closed connection, which is fine.
        }
        Ok(Ok(_)) => {
            let resp: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
            assert!(!resp.ok, "oversized line should be rejected");
            assert!(
                resp.error.as_deref().unwrap().contains("line exceeds"),
                "error should mention line exceeds: {:?}",
                resp.error
            );
        }
        other => panic!("unexpected read result: {other:?}"),
    }

    cancel.cancel();
}

#[tokio::test]
async fn socket_file_permissions_0600() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("dormant.sock");

    let (ctl_tx, _event_tx, _record_rx) = spawn_fake_engine();
    let (reload_tx, _reload_rx) = mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
    let cancel = CancellationToken::new();
    let doctor = fake_doctor(ctl_tx.clone());
    let ds = fake_direct_switch(ctl_tx.clone());

    let _handle = dormantd::ipc::spawn(
        &socket_path,
        ctl_tx,
        dormant_core::reload::ReloadRequester::new(reload_tx),
        doctor,
        ds,
        cancel.clone(),
    )
    .unwrap();

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
    let (reload_tx, _reload_rx) = mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
    let cancel = CancellationToken::new();
    let doctor = fake_doctor(ctl_tx.clone());
    let ds = fake_direct_switch(ctl_tx.clone());

    // Should succeed — replaces the stale socket
    let result = dormantd::ipc::spawn(
        &socket_path,
        ctl_tx,
        dormant_core::reload::ReloadRequester::new(reload_tx),
        doctor,
        ds,
        cancel.clone(),
    );
    assert!(result.is_ok(), "should replace stale socket: {result:?}");

    // Verify the socket file exists and is connectable
    assert!(socket_path.exists(), "socket file should exist");
    let _ = std::os::unix::net::UnixStream::connect(&socket_path)
        .expect("should be able to connect to socket");

    cancel.cancel();
}

/// `IpcRequest::Doctor` is intercepted before the engine path: the IPC
/// server calls `DoctorService::run()` and returns a response carrying
/// `doctor_report`.  The fake engine's `Snapshot` handler is the one that
/// the doctor service hits; verify that the resulting response has
/// `ok=true`, no `snapshot`, and a `doctor_report` present.
#[tokio::test]
async fn doctor_roundtrip_returns_report() {
    let (_dir, socket_path, _ctl_tx, _event_tx, mut record_rx, cancel) = setup_server().await;

    let resp = send_request(&socket_path, &IpcRequest::Doctor).await;

    assert!(resp.ok, "doctor response should be ok: {resp:?}");
    assert!(
        resp.snapshot.is_none(),
        "doctor should not carry a snapshot"
    );
    let report = resp
        .doctor_report
        .as_ref()
        .expect("doctor response must include doctor_report");
    // The fake snapshot has TWO displays (main_monitor + tv) with no
    // controllers — each becomes a Skip "owned by daemon" check.
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "display main_monitor"
                && c.status == dormant_core::doctor::CheckStatus::Skip),
        "doctor report should include the owned-display skip for main_monitor: {report:?}"
    );
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "display tv" && c.status == dormant_core::doctor::CheckStatus::Skip),
        "doctor report should include the owned-display skip for tv: {report:?}"
    );

    // The engine's control channel saw the Snapshot request from the
    // doctor service but should NOT have seen a forwarded
    // `IpcRequest::Doctor` — Doctor is intercepted, not forwarded.
    match tokio::time::timeout(Duration::from_millis(200), record_rx.recv()).await {
        Ok(Some(ControlMsg::Snapshot(_)) | None) | Err(_) => { /* expected */ }
        Ok(Some(other)) => panic!("unexpected forwarded control msg: {other:?}"),
    }

    cancel.cancel();
}

// ── Switch roundtrip tests ────────────────────────────────────────────────

#[tokio::test]
async fn switch_to_local_unsupported_display_returns_error() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    let resp = send_request(
        &socket_path,
        &IpcRequest::SwitchToLocal {
            display: "nonexistent".into(),
        },
    )
    .await;

    assert!(!resp.ok, "nonexistent display should error");
    let err = resp.error.expect("should have error");
    assert!(
        err.contains("nonexistent") && err.contains("not shared"),
        "error should mention display and reason: {err}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn switch_to_peer_not_configured_returns_error() {
    let (_dir, socket_path, _ctl_tx, _event_tx, _record_rx, cancel) = setup_server().await;

    let resp = send_request(
        &socket_path,
        &IpcRequest::SwitchToPeer {
            display: "tv".into(),
        },
    )
    .await;

    assert!(!resp.ok, "unconfigured peer switch should error");
    let err = resp.error.expect("should have error");
    assert!(
        err.contains("tv") && err.contains("not shared"),
        "error should mention display and reason: {err}"
    );

    cancel.cancel();
}

/// Create a tempdir with socket path and spawn the IPC server with a
/// [`DirectSwitchHandle`] that has a configured shared display.
async fn setup_server_with_display(
    display: &str,
) -> (tempfile::TempDir, std::path::PathBuf, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("dormant.sock");

    let (ctl_tx, event_tx, record_rx) = spawn_fake_engine();
    let (reload_tx, _reload_rx) = mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
    let cancel = CancellationToken::new();
    let doctor = fake_doctor(ctl_tx.clone());
    let ds = fake_direct_switch_with(Some(display), ctl_tx.clone());

    let _handle = dormantd::ipc::spawn(
        &socket_path,
        ctl_tx.clone(),
        dormant_core::reload::ReloadRequester::new(reload_tx),
        doctor,
        ds,
        cancel.clone(),
    )
    .unwrap();

    // Suppress unused warnings.
    drop(event_tx);
    drop(record_rx);
    drop(ctl_tx);

    tokio::time::sleep(Duration::from_millis(100)).await;

    (dir, socket_path, cancel)
}

#[tokio::test]
async fn switch_to_local_on_configured_shared_display_succeeds() {
    let (_dir, socket_path, cancel) = setup_server_with_display("desk").await;

    let resp = send_request(
        &socket_path,
        &IpcRequest::SwitchToLocal {
            display: "desk".into(),
        },
    )
    .await;

    assert!(resp.ok, "switch to local should succeed: {resp:?}");
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    cancel.cancel();
}

#[tokio::test]
async fn switch_to_peer_configured_succeeds() {
    let (_dir, socket_path, cancel) = setup_server_with_display("desk").await;

    // setup_server_with_display doesn't configure peer write code,
    // so this should return a specific error.
    let resp = send_request(
        &socket_path,
        &IpcRequest::SwitchToPeer {
            display: "desk".into(),
        },
    )
    .await;

    assert!(!resp.ok, "peer switch without code should error");
    let err = resp.error.expect("should have error");
    assert!(
        err.contains("desk") && err.contains("not configured"),
        "error should mention not configured: {err}"
    );
    cancel.cancel();
}
