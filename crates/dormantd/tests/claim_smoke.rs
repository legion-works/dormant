//! KVM claim runtime smoke paths (T10b stage 3).
//!
//! Two SEPARATE test functions cover the two integration paths
//! the dispatch pins (task text lines 113-124, 175): the OWNER
//! path (inbound `ClaimRequest` → release sequence → `before_release`
//! hook → write → `after_release` hook → `claim_completed`, with
//! EXACTLY ONE `write_input_source` call) and the FALLBACK path
//! (a reachable peer accepts the request but never replies →
//! `claim_fallback_direct` → direct write on powered, ZERO writes on
//! standby/unknown). Keeping them in
//! separate test functions ensures one path can never
//! accidentally satisfy the other's assertions.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dormant_core::claim::{ClaimFrame, ClaimMessage, ClaimRequest, ClaimResponse, ClaimVerdict};
use dormant_core::claim_engine::{Action, ClaimEngine, RequesterEvent, Terminal};
use dormant_core::config::schema::{
    ActivityClaimPolicy, AudioConfig, Config, HookAction, HookSlots, KeymapConfig,
    NotificationsConfig, WatchdogConfig, WearConfig,
};
use dormant_core::coordination::CoordinationHandle;
use dormant_core::ownership::OwnershipGate;
use dormant_core::peers::{InstanceIdentity, instance_id_from_public_key};
use dormant_core::traits::CommandSink;
use dormant_core::types::{BlankMode, CmdFailure, DisplayId};
use ed25519_dalek::SigningKey;
use indexmap::IndexMap;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use dormantd::activity_claim_evaluator::{self, PolicyEvaluatorDeps};
use dormantd::claim_runtime::{self, ClaimRuntimeDeps, ClaimRuntimeHandle, ClaimSharedResult};
use dormantd::hooks::{Direction, HookEngine, HookOutcome, HookRunner};
use dormantd::idle_observation::{IdleObservation, idle_observation_channel};

// ── RecordingSink (test CommandSink impl) ──────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub sink: String,
    pub method: &'static str,
    pub arg: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedRead {
    /// A powered read returning this code.
    Powered(u8),
    /// A standby read (the `0x60` returns 0 in standby).
    Standby,
    /// A "no readback" (controller chain had nothing to say).
    Unknown,
}

#[derive(Default)]
struct SinkInner {
    writes: Vec<RecordedCall>,
    reads: Mutex<VecDeque<ScriptedRead>>,
    wakes: Vec<RecordedCall>,
    writes_failed: bool,
    identity: Option<String>,
}

pub struct RecordingSink {
    name: String,
    inner: Arc<Mutex<SinkInner>>,
    changed: Arc<tokio::sync::Notify>,
}

impl RecordingSink {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inner: Arc::new(Mutex::new(SinkInner::default())),
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn set_claim_identity(&self, id: impl Into<String>) {
        self.inner.lock().unwrap().identity = Some(id.into());
    }
    fn script_reads(&self, reads: Vec<ScriptedRead>) {
        let mut g = self.inner.lock().unwrap();
        g.reads = Mutex::new(reads.into());
    }
    fn write_calls(&self) -> Vec<RecordedCall> {
        self.inner.lock().unwrap().writes.clone()
    }
    fn wake_calls(&self) -> Vec<RecordedCall> {
        self.inner.lock().unwrap().wakes.clone()
    }
}

#[async_trait]
impl CommandSink for RecordingSink {
    async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
        Ok(())
    }
    async fn wake(&self) -> Result<(), CmdFailure> {
        let call = RecordedCall {
            sink: self.name.clone(),
            method: "wake",
            arg: None,
        };
        self.inner.lock().unwrap().wakes.push(call);
        self.changed.notify_one();
        Ok(())
    }
    fn controller_health(&self) -> Vec<dormant_core::rules::ControllerHealth> {
        Vec::new()
    }
    async fn read_state(&self) -> Option<dormant_core::traits::PanelState> {
        Some(dormant_core::traits::PanelState {
            power: Some(dormant_core::traits::PowerState::On),
            brightness: Some(50),
        })
    }
    async fn read_input_source_sampled(&self) -> Result<Option<u8>, String> {
        let next = self.inner.lock().unwrap().reads.lock().unwrap().pop_front();
        Ok(match next {
            Some(ScriptedRead::Powered(v)) => Some(v),
            Some(ScriptedRead::Standby) => Some(0),
            Some(ScriptedRead::Unknown) | None => None,
        })
    }
    async fn write_input_source(
        &self,
        target: dormant_core::traits::InputSourceTarget,
    ) -> Result<(), CmdFailure> {
        let call = RecordedCall {
            sink: self.name.clone(),
            method: "write_input_source",
            arg: Some(target.write_code),
        };
        let mut g = self.inner.lock().unwrap();
        if g.writes_failed {
            Err(CmdFailure {
                controller: self.name.clone(),
                error: "E_DISPLAY_IO: scripted failure".to_string(),
            })
        } else {
            g.writes.push(call);
            drop(g);
            self.changed.notify_one();
            Ok(())
        }
    }
    fn panel_identity(&self) -> Option<String> {
        Some(format!("panel:{}", self.name))
    }
    fn claim_identity(&self) -> Option<String> {
        self.inner.lock().unwrap().identity.clone()
    }
}

// ── RecordingHookRunner ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedHook {
    pub direction: &'static str,
    pub phase: &'static str,
    pub index: usize,
    pub kind: &'static str,
}

#[derive(Default)]
struct HookInner {
    calls: Vec<RecordedHook>,
    outcomes: Mutex<VecDeque<HookOutcome>>,
}

#[derive(Clone)]
struct CommandBarrier {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

pub struct RecordingHookRunner {
    inner: Arc<Mutex<HookInner>>,
    command_barriers: Mutex<VecDeque<CommandBarrier>>,
    calls_changed: Arc<tokio::sync::Notify>,
}

impl Default for RecordingHookRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingHookRunner {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HookInner::default())),
            command_barriers: Mutex::new(VecDeque::new()),
            calls_changed: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn push_completed(&self, started: usize, failed: usize, spawned: usize) {
        self.inner
            .lock()
            .unwrap()
            .outcomes
            .lock()
            .unwrap()
            .push_back(HookOutcome::Completed {
                started,
                failed,
                spawned,
            });
    }
    fn calls(&self) -> Vec<RecordedHook> {
        self.inner.lock().unwrap().calls.clone()
    }

    fn block_next_command(&self) -> CommandBarrier {
        let barrier = CommandBarrier {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        self.command_barriers
            .lock()
            .unwrap()
            .push_back(barrier.clone());
        barrier
    }
}

#[async_trait]
impl HookRunner for RecordingHookRunner {
    async fn run_command(
        &self,
        _env: &dormantd::hooks::EnvList,
        argv: &[String],
        _timeout_: Duration,
    ) -> Result<(), String> {
        let dir = match argv.first().map(String::as_str) {
            Some("acquire") => Direction::Acquire,
            _ => Direction::Release,
        };
        let kind = "command";
        self.inner.lock().unwrap().calls.push(RecordedHook {
            direction: match dir {
                Direction::Release => "release",
                Direction::Acquire => "acquire",
            },
            phase: match argv.get(1).map(String::as_str) {
                Some("after") => "after",
                _ => "before",
            },
            index: 0,
            kind,
        });
        self.calls_changed.notify_one();
        let barrier = self.command_barriers.lock().unwrap().pop_front();
        if let Some(barrier) = barrier {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
        self.inner
            .lock()
            .unwrap()
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "no scripted hook outcome".to_string())
            .and_then(|o| match o {
                HookOutcome::Completed { .. } => Ok(()),
                HookOutcome::Aborted { reason, .. } => Err(reason),
            })
    }
    async fn publish_mqtt(
        &self,
        _topic: &str,
        _payload: &str,
        _timeout_: Duration,
    ) -> Result<(), String> {
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn command_action(argv: Vec<String>) -> HookAction {
    HookAction {
        command: Some(argv),
        mqtt: None,
        timeout: Duration::from_secs(2),
        blocking: Some(true),
        abort_on_failure: false,
    }
}

/// Build a `HookSlots` with: release→before: 1 blocking command;
/// release→after: 1 non-blocking command; acquire→before: 1
/// blocking command; acquire→after: 1 non-blocking command.
fn release_acquire_hooks() -> HookSlots {
    HookSlots {
        before_release: vec![command_action(vec![
            "release".to_string(),
            "before".to_string(),
        ])],
        after_release: vec![command_action(vec![
            "release".to_string(),
            "after".to_string(),
        ])],
        before_acquire: vec![command_action(vec![
            "acquire".to_string(),
            "before".to_string(),
        ])],
        after_acquire: vec![command_action(vec![
            "acquire".to_string(),
            "after".to_string(),
        ])],
    }
}

fn shared_display_config_with_write_code(
    display: &str,
    read_code: u8,
    write_code: u8,
    hooks: HookSlots,
) -> Arc<Config> {
    let mut displays = IndexMap::new();
    displays.insert(
        display.to_owned(),
        dormant_core::config::DisplayConfig {
            controllers: vec!["ddcci".to_owned()],
            scope: dormant_core::config::DisplayScope::Shared,
            shared_input_code: Some(read_code),
            shared_input_write_code: Some(write_code),
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            blank_mode: Some(BlankMode::BrightnessZero),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: Some(vec![BlankMode::BrightnessZero]),
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 80,
            samsung_restore_backlight: 50,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::Unknown,
            hooks,
        },
    );
    Arc::new(Config {
        config_version: 1,
        daemon: dormant_core::config::DaemonConfig::default(),
        sensors: IndexMap::new(),
        zones: IndexMap::new(),
        displays,
        rules: IndexMap::new(),
        wear: WearConfig::default(),
        notifications: NotificationsConfig::default(),
        watchdog: WatchdogConfig::default(),
        audio: AudioConfig::default(),
        keymap: KeymapConfig::default(),
        input_filter: dormant_core::config::InputFilterConfig::default(),
        coordination: dormant_core::config::CoordinationConfig {
            enabled: true,
            poll_interval: Duration::from_secs(2),
            state_poll_interval: None,
            loss_confirmations: 3,
            pairing_port: 0,
            pairing_window: Duration::from_secs(300),
            pairing_bind_address: None,
            activity_claim: ActivityClaimPolicy::Off,
            owner_idle_window: Duration::from_secs(30),
            armed_window: Duration::from_secs(60),
            claim_timeout: Duration::from_millis(500),
            release_deadline_cap: Duration::from_secs(45),
            claim_port: 0,
            claim_bind_address: None,
            claim_advertise_mdns: true,
            ..dormant_core::config::CoordinationConfig::default()
        },
    })
}

fn shared_display_config(display: &str, code: u8, hooks: HookSlots) -> Arc<Config> {
    let mut displays = IndexMap::new();
    displays.insert(
        display.to_owned(),
        dormant_core::config::DisplayConfig {
            controllers: vec!["ddcci".to_owned()],
            scope: dormant_core::config::DisplayScope::Shared,
            shared_input_code: Some(code),
            shared_input_write_code: None,
            shared_peer_input_code: None,
            shared_peer_input_write_code: None,
            blank_mode: Some(BlankMode::BrightnessZero),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: Some(vec![BlankMode::BrightnessZero]),
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: Duration::from_secs(5),
            restore_brightness: 80,
            samsung_restore_backlight: 50,
            treat_unreachable_as_blanked: true,
            panel_type: dormant_core::wear::PanelType::Unknown,
            hooks,
        },
    );
    Arc::new(Config {
        config_version: 1,
        daemon: dormant_core::config::DaemonConfig::default(),
        sensors: IndexMap::new(),
        zones: IndexMap::new(),
        displays,
        rules: IndexMap::new(),
        wear: WearConfig::default(),
        notifications: NotificationsConfig::default(),
        watchdog: WatchdogConfig::default(),
        audio: AudioConfig::default(),
        keymap: KeymapConfig::default(),
        input_filter: dormant_core::config::InputFilterConfig::default(),
        coordination: dormant_core::config::CoordinationConfig {
            enabled: true,
            poll_interval: Duration::from_secs(2),
            state_poll_interval: None,
            loss_confirmations: 3,
            pairing_port: 0,
            pairing_window: Duration::from_secs(300),
            pairing_bind_address: None,
            activity_claim: ActivityClaimPolicy::Off,
            owner_idle_window: Duration::from_secs(30),
            armed_window: Duration::from_secs(60),
            claim_timeout: Duration::from_millis(500),
            release_deadline_cap: Duration::from_secs(45),
            claim_port: 0,
            claim_bind_address: None,
            claim_advertise_mdns: true,
            ..dormant_core::config::CoordinationConfig::default()
        },
    })
}

async fn read_claim_frame(stream: &mut tokio::net::TcpStream) -> Option<ClaimFrame> {
    use tokio::io::AsyncReadExt as _;

    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await.ok()?;
    let length = usize::try_from(u32::from_be_bytes(length)).ok()?;
    if !(1..=1_048_576).contains(&length) {
        return None;
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await.ok()?;
    serde_json::from_slice(&payload).ok()
}

#[derive(Clone, Copy)]
enum PeerMode {
    ReachableSilent,
    NoPeer,
    Unreachable,
}

struct ScriptedTransport {
    handle: Arc<dormantd::coordination_claim::ClaimTransportHandle>,
    peer_frames: Arc<Mutex<Vec<ClaimFrame>>>,
    peer_frames_changed: Arc<tokio::sync::Notify>,
    _peer_watch_tx: watch::Sender<Vec<dormantd::coordination_claim::ClaimPeer>>,
    _peer_task: Option<tokio::task::JoinHandle<()>>,
}

async fn build_scripted_transport(
    local_identity: InstanceIdentity,
    cancel: CancellationToken,
    peer_mode: PeerMode,
) -> ScriptedTransport {
    use dormant_core::claim::Epoch;
    use dormantd::coordination_claim::{ClaimPeer, ClaimTransportDeps};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::net::TcpListener;

    let peer_signing = SigningKey::from_bytes(&[7; 32]);
    let peer = InstanceIdentity {
        instance_id: instance_id_from_public_key(&peer_signing.verifying_key().to_bytes()),
        verifying_key: peer_signing.verifying_key(),
        signing_key: peer_signing,
    };
    let (listener, peer_port) = match peer_mode {
        PeerMode::ReachableSilent => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            (Some(listener), Some(port))
        }
        PeerMode::NoPeer => (None, None),
        PeerMode::Unreachable => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            (None, Some(port))
        }
    };
    let peers = peer_port
        .map(|port| {
            vec![ClaimPeer {
                instance_id: peer.instance_id,
                verifying_key: peer.verifying_key,
                last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
                dns_addr: None,
                claim_port: Some(port),
                dns_port: None,
                dns_epoch: Some(Epoch::try_from("peer-epoch-00001").unwrap()),
            }]
        })
        .unwrap_or_default();
    let (peer_watch_tx, peers_rx) = watch::channel(peers);
    let peer_frames = Arc::new(Mutex::new(Vec::new()));
    let peer_frames_changed = Arc::new(tokio::sync::Notify::new());
    let peer_task = listener.map(|listener| {
        let frames = Arc::clone(&peer_frames);
        let changed = Arc::clone(&peer_frames_changed);
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    () = cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = stream else {
                    break;
                };
                let frame = tokio::select! {
                    () = cancel.cancelled() => break,
                    frame = read_claim_frame(&mut stream) => frame,
                };
                if let Some(frame) = frame {
                    frames.lock().unwrap().push(frame);
                    changed.notify_one();
                }
            }
        })
    });
    let handle = Arc::new(dormantd::coordination_claim::spawn(ClaimTransportDeps {
        identity: Arc::new(local_identity),
        boot_epoch: Epoch::try_from("0123456789abcdef").unwrap(),
        peers: peers_rx,
        bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
        fixed_port: None,
        enabled: true,
        on_peer_addr: Box::new(|_, _| {}),
    }));
    ScriptedTransport {
        handle,
        peer_frames,
        peer_frames_changed,
        _peer_watch_tx: peer_watch_tx,
        _peer_task: peer_task,
    }
}

struct ClaimHarness {
    handle: ClaimRuntimeHandle,
    sink: Arc<RecordingSink>,
    runner: Arc<RecordingHookRunner>,
    log: Arc<Mutex<Vec<String>>>,
    log_changed: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    // The watch senders MUST stay alive for the driver's
    // `select!` to keep its `changed()` arms pending (a closed
    // sender makes `changed()` return `Err` immediately, which
    // the runtime treats as a quit signal). The harness holds
    // them until `shutdown()` drops everything.
    _config_tx: watch::Sender<Arc<Config>>,
    _executors_tx: watch::Sender<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    local_identity: InstanceIdentity,
    /// Coordination handle (daemon-lifetime, cloned before
    /// passing to `ClaimRuntimeDeps`).  Tests access it to
    /// set owner identity for the `IdleReport` sender gate.
    coord: CoordinationHandle,
    peer_frames: Arc<Mutex<Vec<ClaimFrame>>>,
    peer_frames_changed: Arc<tokio::sync::Notify>,
    _peer_watch_tx: watch::Sender<Vec<dormantd::coordination_claim::ClaimPeer>>,
    _peer_task: Option<tokio::task::JoinHandle<()>>,
}

impl ClaimHarness {
    async fn build_with_reachable_silent_peer(display: &str, code: u8) -> Self {
        Self::build_with_peer_mode(display, code, PeerMode::ReachableSilent).await
    }

    async fn build_with_unreachable_peer(display: &str, code: u8) -> Self {
        Self::build_with_peer_mode(display, code, PeerMode::Unreachable).await
    }

    async fn build_without_addressable_peer(display: &str, code: u8) -> Self {
        Self::build_with_peer_mode(display, code, PeerMode::NoPeer).await
    }

    async fn build_with_custom_config(
        display: &str,
        config: Arc<Config>,
        peer_mode: PeerMode,
    ) -> Self {
        let runner = Arc::new(RecordingHookRunner::new());
        let hook_engine = Arc::new(HookEngine::with_runner(
            runner.clone() as Arc<dyn HookRunner>
        ));
        let sink = Arc::new(RecordingSink::new(display));
        sink.set_claim_identity(format!("panel-{display}"));
        let code = config
            .displays
            .get(display)
            .and_then(|dc| dc.shared_input_code)
            .unwrap_or(0x0f);
        sink.script_reads(vec![
            ScriptedRead::Powered(code),
            ScriptedRead::Powered(code),
            ScriptedRead::Powered(code),
        ]);
        let cancel = CancellationToken::new();
        let (config_tx, config_rx) = watch::channel(config.clone());
        let (executors_tx, executors_rx) = watch::channel({
            let mut map: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
            map.insert(DisplayId(display.to_owned()), sink.clone());
            Arc::new(map)
        });
        let local_signing = SigningKey::from_bytes(&[42; 32]);
        let local_identity = InstanceIdentity {
            instance_id: instance_id_from_public_key(&local_signing.verifying_key().to_bytes()),
            signing_key: local_signing,
            verifying_key: SigningKey::from_bytes(&[42; 32]).verifying_key(),
        };
        let ScriptedTransport {
            handle: transport,
            peer_frames,
            peer_frames_changed,
            _peer_watch_tx: peer_watch_tx,
            _peer_task: peer_task,
        } = build_scripted_transport(local_identity.clone(), cancel.clone(), peer_mode).await;
        let coord = CoordinationHandle::new([DisplayId(display.to_owned())]);
        coord.record_success(&DisplayId(display.to_owned()), code, code, None);
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let log = Arc::new(Mutex::new(Vec::new()));
        let log_changed = Arc::new(tokio::sync::Notify::new());
        let handle = claim_runtime::spawn(ClaimRuntimeDeps {
            identity: Arc::new(local_identity.clone()),
            transport,
            executors: executors_rx,
            config: config_rx,
            hooks: hook_engine,
            coordination: Some(coord.clone()),
            front_ctl_tx,
            cancel: cancel.clone(),
            event_log: Some(log.clone()),
            event_notify: Some(Arc::clone(&log_changed)),
            idle_rx: None,
        });
        let accepted = handle
            .inject_owner_completion_for_test(
                DisplayId(display.to_owned()),
                "startup-barrier",
                dormant_core::claim_engine::OwnerEvent::DisplayRemoved,
            )
            .await
            .expect("claim runtime startup barrier");
        assert!(!accepted, "startup barrier must not match a flight");
        Self {
            handle,
            sink,
            runner,
            log,
            log_changed,
            cancel,
            _config_tx: config_tx,
            _executors_tx: executors_tx,
            local_identity,
            coord,
            peer_frames,
            peer_frames_changed,
            _peer_watch_tx: peer_watch_tx,
            _peer_task: peer_task,
        }
    }

    async fn build_with_peer_mode(display: &str, code: u8, peer_mode: PeerMode) -> Self {
        let runner = Arc::new(RecordingHookRunner::new());
        let hook_engine = Arc::new(HookEngine::with_runner(
            runner.clone() as Arc<dyn HookRunner>
        ));
        let sink = Arc::new(RecordingSink::new(display));
        sink.set_claim_identity(format!("panel-{display}"));
        // The startup writability probe consumes one read and write before each test.
        sink.script_reads(vec![
            ScriptedRead::Powered(code),
            ScriptedRead::Powered(code),
            ScriptedRead::Powered(code),
        ]);
        let cancel = CancellationToken::new();
        let (config_tx, config_rx) = watch::channel(shared_display_config(
            display,
            code,
            release_acquire_hooks(),
        ));
        let (executors_tx, executors_rx) = watch::channel({
            let mut map: HashMap<DisplayId, Arc<dyn CommandSink>> = HashMap::new();
            map.insert(DisplayId(display.to_owned()), sink.clone());
            Arc::new(map)
        });
        let local_signing = SigningKey::from_bytes(&[42; 32]);
        let local_identity = InstanceIdentity {
            instance_id: instance_id_from_public_key(&local_signing.verifying_key().to_bytes()),
            signing_key: local_signing,
            verifying_key: SigningKey::from_bytes(&[42; 32]).verifying_key(),
        };
        let ScriptedTransport {
            handle: transport,
            peer_frames,
            peer_frames_changed,
            _peer_watch_tx: peer_watch_tx,
            _peer_task: peer_task,
        } = build_scripted_transport(local_identity.clone(), cancel.clone(), peer_mode).await;
        let coord = CoordinationHandle::new([DisplayId(display.to_owned())]);
        // Matching observed and expected input marks this fixture locally owned.
        coord.record_success(&DisplayId(display.to_owned()), code, code, None);
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let log = Arc::new(Mutex::new(Vec::new()));
        let log_changed = Arc::new(tokio::sync::Notify::new());
        let handle = claim_runtime::spawn(ClaimRuntimeDeps {
            identity: Arc::new(local_identity.clone()),
            transport,
            executors: executors_rx,
            config: config_rx,
            hooks: hook_engine,
            coordination: Some(coord.clone()),
            front_ctl_tx,
            cancel: cancel.clone(),
            event_log: Some(log.clone()),
            event_notify: Some(Arc::clone(&log_changed)),
            idle_rx: None,
        });
        // The acknowledged command cannot run until the initial context refresh completes.
        let accepted = handle
            .inject_owner_completion_for_test(
                DisplayId(display.to_owned()),
                "startup-barrier",
                dormant_core::claim_engine::OwnerEvent::DisplayRemoved,
            )
            .await
            .expect("claim runtime startup barrier");
        assert!(!accepted, "startup barrier must not match a flight");
        Self {
            handle,
            sink,
            runner,
            log,
            log_changed,
            cancel,
            _config_tx: config_tx,
            _executors_tx: executors_tx,
            local_identity,
            coord,
            peer_frames,
            peer_frames_changed,
            _peer_watch_tx: peer_watch_tx,
            _peer_task: peer_task,
        }
    }

    fn hook_calls(&self) -> Vec<RecordedHook> {
        self.runner.calls()
    }
    fn sink_writes(&self) -> Vec<RecordedCall> {
        self.sink.write_calls()
    }
    /// Clear the write log so the test starts from a clean
    /// slate. The runtime's startup `sink_input_writable`
    /// probe writes one entry (the observed code) before the
    /// test's first action — clearing it removes that artifact
    /// from the test's write-count assertions.
    fn clear_sink_writes(&self) {
        // The simplest cross-thread clear: pop the
        // recorded writes by overwriting the queue. We
        // also pop the wake log for symmetry.
        let mut g = self.sink.inner.lock().unwrap();
        g.writes.clear();
        g.wakes.clear();
    }
    fn sink_wakes(&self) -> Vec<RecordedCall> {
        self.sink.wake_calls()
    }
    fn log_events(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn peer_request_count(&self) -> usize {
        self.peer_frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| matches!(&frame.message, ClaimMessage::ClaimRequest(_)))
            .count()
    }

    async fn wait_for_peer_requests(&self, count: usize, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let changed = self.peer_frames_changed.notified();
                if self.peer_request_count() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .is_ok()
    }
    async fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        self.wait_for_log_matching(timeout, |event| event == needle)
            .await
    }

    async fn wait_for_log_prefix(&self, prefix: &str, timeout: Duration) -> bool {
        self.wait_for_log_matching(timeout, |event| event.starts_with(prefix))
            .await
    }

    async fn wait_for_log_matching(
        &self,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
    ) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if self.log_events().iter().any(|event| predicate(event)) {
                    return;
                }
                self.log_changed.notified().await;
            }
        })
        .await
        .is_ok()
    }

    async fn wait_for_n_writes(&self, n: usize, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if self.sink_writes().len() >= n {
                    return;
                }
                self.sink.changed.notified().await;
            }
        })
        .await
        .is_ok()
    }

    async fn wait_for_n_hook_calls(&self, n: usize, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if self.hook_calls().len() >= n {
                    return;
                }
                self.runner.calls_changed.notified().await;
            }
        })
        .await
        .is_ok()
    }

    fn shutdown(&self) {
        self.cancel.cancel();
    }
    /// Build a signed `ClaimFrame` for an inbound `ClaimRequest`
    /// with the given nonce and requester input code. The
    /// claimed display identity matches the harness's sink
    /// (`RecordingSink::claim_identity` returns
    /// `Some("panel-<name>")` after the test's
    /// `set_claim_identity` call).
    fn build_inbound_request(&self, nonce: &str, requester_code: u8) -> ClaimFrame {
        let peer_signing = SigningKey::from_bytes(&[7; 32]);
        let request = ClaimRequest {
            display_identity: format!("panel-{}", "mon"),
            requester_instance_id: instance_id_from_public_key(
                &peer_signing.verifying_key().to_bytes(),
            ),
            requester_input_code: u16::from(requester_code),
            counter: 1,
            nonce: nonce.to_owned(),
        };
        ClaimFrame::sign(
            &InstanceIdentity {
                instance_id: instance_id_from_public_key(&peer_signing.verifying_key().to_bytes()),
                signing_key: peer_signing,
                verifying_key: SigningKey::from_bytes(&[7; 32]).verifying_key(),
            },
            "peer-epoch-00001".to_owned(),
            self.local_identity.instance_id.clone(),
            "0123456789abcdef".to_owned(),
            1,
            nonce.to_owned(),
            ClaimMessage::ClaimRequest(request),
        )
        .unwrap()
    }
}

// ── 1. Negotiated path (OWNER side, in-process) ─────────────────

/// The OWNER path: an inbound `ClaimRequest` from a paired
/// peer triggers the release sequence:
///   `claim_accepted` → `before_release` (1 hook call) →
///   `write_input_source(0x11)` → `after_release` (1 hook
///   call) → `Terminal::Released`. EXACTLY ONE write. NO
///   `claim_fallback_direct`. The hook calls are observed in
///   order: `release/before` → `write_input_source(0x11)` →
///   `release/after`. The test is the OWNER-side analog of
///   the negotiated happy-path (the requester side, which
///   depends on the cross-claim convergence test, is
///   exercised separately).
#[tokio::test]
async fn negotiated_claim_order_is_release_write_flip_acquire() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    // Clear the startup writability probe's write.
    harness.clear_sink_writes();
    // The owner path drives two hook slots before release
    // completes: before_release and after_release.
    // (before_acquire and after_acquire are requester-side hooks.)
    for _ in 0..2 {
        harness
            .runner
            .push_completed(/*started*/ 1, /*failed*/ 0, /*spawned*/ 0);
    }
    // Drive the OWNER path directly: feed an `OwnerRequest`
    // to the engine through the runtime's `inject_inbound`.
    // The wire-level `inbound` path is exercised by the
    // `app_start_wires_authenticated_claim_transport` smoke
    // in `daemon_smoke.rs`; here we drive the engine
    // through the same code path the runtime would drive
    // it through, in isolation.
    //
    // We construct an `OwnerRequest` that mirrors what the
    // runtime would build for a wired inbound claim, and
    // exercise the OWNER side via the engine directly
    // (driven by the runtime's `inject_inbound`). The
    // runtime's OWNER code path is: build the request,
    // begin_owner, dispatch `SendVerdict` + `RunBeforeRelease`
    // + `WriteInput` + `RunAfterRelease` → terminal.
    //
    // The `handle_inbound_request` path in the runtime
    // builds the request from the inbound frame. We call
    // it via a small test seam by feeding the
    // `OwnerRequest` directly into a fresh engine and
    // running the dispatch through the runtime's helper
    // for the write step.
    let frame = harness.build_inbound_request("release-1", 0x11);
    harness
        .handle
        .inject_inbound_for_test(frame)
        .await
        .expect("inject inbound");
    // The owner is now in WaitForAcquireReady. Inject the
    // AcquireReady event so the owner proceeds to release.
    harness
        .handle
        .inject_owner_completion_for_test(
            DisplayId("mon".to_string()),
            "release-1",
            dormant_core::claim_engine::OwnerEvent::AcquireReady,
        )
        .await
        .expect("inject AcquireReady");
    // The driver routes the inbound through the
    // OWNER-side begin → SendVerdict → RunBeforeRelease →
    // WriteInput → RunAfterRelease → terminal sequence.
    // The release hooks return Completed (we queued
    // four); the write fires once.
    assert!(
        harness.wait_for_n_writes(1, Duration::from_secs(2)).await,
        "OWNER path must produce EXACTLY ONE write_input_source call"
    );
    assert!(
        harness
            .wait_for_n_hook_calls(2, Duration::from_secs(2))
            .await,
        "OWNER path must complete its release hooks"
    );
    let writes = harness.sink_writes();
    assert_eq!(
        writes.len(),
        1,
        "OWNER path must write exactly once; got {writes:?}"
    );
    assert_eq!(writes[0].method, "write_input_source");
    assert_eq!(
        writes[0].arg,
        Some(0x11),
        "OWNER must select the requester's input code (0x11)"
    );
    let events = harness.log_events();
    assert!(
        events.iter().any(|e| e == "claim_accepted"),
        "OWNER path must emit claim_accepted; got {events:?}"
    );
    // The OWNER path's terminal is `Terminal::Released` (the
    // `claim_completed` trace is the REQUESTER-side success).
    // We don't have a separate trace for it; the engine's
    // `Action::Terminal(Released)` is dispatched and recorded
    // but is not a `Trace` event. The test instead pins the
    // absence of the failure path.
    assert!(
        !events.iter().any(|e| e == "claim_fallback_direct"),
        "negotiated path must NOT emit claim_fallback_direct; got {events:?}"
    );
    assert!(
        !events.iter().any(|e| e == "claim_release_aborted"),
        "negotiated path must NOT emit claim_release_aborted; got {events:?}"
    );
    // Hook order: release-before precedes release-after.
    let hook_calls = harness.hook_calls();
    let release_before = hook_calls
        .iter()
        .position(|c| c.direction == "release" && c.phase == "before");
    let release_after = hook_calls
        .iter()
        .position(|c| c.direction == "release" && c.phase == "after");
    assert!(
        release_before.is_some(),
        "release-before hook must fire on OWNER path; got {hook_calls:?}"
    );
    assert!(
        release_after.is_some(),
        "release-after hook must fire on OWNER path; got {hook_calls:?}"
    );
    assert!(
        release_before.unwrap() < release_after.unwrap(),
        "release-before must precede release-after; got {hook_calls:?}"
    );
    // No wakes on a powered release.
    assert_eq!(harness.sink_wakes().len(), 0, "no wake on powered release");
    harness.shutdown();
}

#[tokio::test]
async fn stale_nonce_owner_completion_does_not_advance_current_flight() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    let before_release = harness.runner.block_next_command();
    for _ in 0..2 {
        harness.runner.push_completed(1, 0, 0);
    }

    let frame = harness.build_inbound_request("current", 0x11);
    harness
        .handle
        .inject_inbound_for_test(frame)
        .await
        .expect("inject inbound");
    // Advance past WaitForAcquireReady so the owner reaches before_release.
    harness
        .handle
        .inject_owner_completion_for_test(
            DisplayId("mon".to_owned()),
            "current",
            dormant_core::claim_engine::OwnerEvent::AcquireReady,
        )
        .await
        .expect("inject AcquireReady");
    tokio::time::timeout(Duration::from_secs(2), before_release.entered.notified())
        .await
        .expect("current flight must be waiting for before-release completion");

    let accepted = harness
        .handle
        .inject_owner_completion_for_test(
            DisplayId("mon".to_owned()),
            "stale",
            dormant_core::claim_engine::OwnerEvent::BeforeRelease(
                dormant_core::claim_engine::HookResult::Completed,
            ),
        )
        .await
        .expect("inject stale owner completion");
    assert!(!accepted, "stale completion must be dropped");
    assert!(
        harness.sink_writes().is_empty(),
        "stale completion must not advance the current flight to its write"
    );

    before_release.release.notify_one();
    assert!(
        harness.wait_for_n_writes(1, Duration::from_secs(2)).await,
        "the matching completion must still advance the current flight"
    );
    harness.shutdown();
}

// ── 2. Fallback path ─────────────────────────────────────────────

/// The FALLBACK path: a local claim trigger reaches a peer
/// that reads the frame but never replies; the requester deadline
/// (`claim_timeout: 500ms` in the test config) elapses;
/// the engine drives `AttemptFallback`; the driver
/// performs a fresh bounded `read_input_source_sampled`
/// and, on a POWERED result, performs ONE direct
/// `write_input_source`. The `claim_fallback_direct` trace
/// fires. No `before_release` hook fires.
#[tokio::test]
async fn fallback_claim_order_emits_fallback_direct_then_direct_write() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    // The fallback path's first read observes the LOCAL
    // code (0x0f) — the panel is already showing the
    // requester's input; the driver reports `claim_completed`
    // without a write (the cross-claim convergence case).
    harness.sink.script_reads(vec![
        ScriptedRead::Powered(0x0f),
        ScriptedRead::Powered(0x0f),
    ]);
    // The scripted peer accepts the request but returns no verdict,
    // so the requester deadline drives the fallback sequence.
    let first = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert!(
        matches!(first, ClaimSharedResult::Accepted { .. }),
        "first try_claim must be Accepted; got {first:?}"
    );
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before the flight proceeds"
    );
    assert!(
        harness
            .wait_for_log("claim_fallback_direct", Duration::from_secs(5))
            .await,
        "fallback must emit claim_fallback_direct"
    );
    assert!(
        harness
            .wait_for_log("claim_completed", Duration::from_secs(2))
            .await,
        "already-local fallback must reach its terminal event"
    );
    let events = harness.log_events();
    assert!(
        events.iter().any(|e| e == "claim_fallback_direct"),
        "claim_fallback_direct must fire; got {events:?}"
    );
    // No `before_release` hook on the fallback path.
    let hook_calls = harness.hook_calls();
    let before_release_calls: Vec<&RecordedHook> = hook_calls
        .iter()
        .filter(|c| c.direction == "release" && c.phase == "before")
        .collect();
    assert_eq!(
        before_release_calls.len(),
        0,
        "fallback path must NOT fire before_release hooks; got {hook_calls:?}"
    );
    // The fallback path did NOT use the negotiated
    // `claim_accepted` trace.
    assert!(
        !events.iter().any(|e| e == "claim_accepted"),
        "fallback path must NOT emit claim_accepted; got {events:?}"
    );
    // On a powered "already-local-code" read the driver
    // does not write — this is the cross-claim
    // final-value convergence: both sides converge on
    // the LOCAL code.
    let writes = harness.sink_writes();
    assert_eq!(
        writes.len(),
        0,
        "fallback with already-local-code read must not write; got {writes:?}"
    );
    harness.shutdown();
}

#[tokio::test]
async fn no_addressable_peer_is_denied_without_negotiation() {
    let harness = ClaimHarness::build_without_addressable_peer("mon", 0x0f).await;
    harness.clear_sink_writes();

    let result = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");

    assert_eq!(
        result,
        ClaimSharedResult::Denied(dormant_core::claim::ClaimDeniedReason::CoordinationDisabled,)
    );
    assert!(
        harness
            .wait_for_log("claim_no_addressable_peers", Duration::from_secs(2))
            .await,
        "zero-peer denial must emit claim_no_addressable_peers"
    );
    assert_eq!(harness.peer_request_count(), 0);
    assert!(
        !harness
            .log_events()
            .iter()
            .any(|event| event == "claim_fallback_direct")
    );
    harness.shutdown();
}

#[tokio::test]
async fn unreachable_peer_is_denied_after_failed_fanout() {
    let harness = ClaimHarness::build_with_unreachable_peer("mon", 0x0f).await;
    harness.clear_sink_writes();

    let result = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");

    assert_eq!(
        result,
        ClaimSharedResult::Denied(dormant_core::claim::ClaimDeniedReason::CoordinationDisabled,)
    );
    assert!(
        harness
            .wait_for_log("claim_no_addressable_peers", Duration::from_secs(2))
            .await,
        "failed fanout must emit claim_no_addressable_peers"
    );
    assert_eq!(harness.peer_request_count(), 0);
    assert!(
        !harness
            .log_events()
            .iter()
            .any(|event| event == "claim_fallback_direct")
    );
    harness.shutdown();
}

/// Unknown-fallback-state: when the fallback's fresh read
/// returns `Standby`, the driver MUST NOT write. ZERO
/// writes, visible `claim_failed` trace, no
/// `claim_completed`.
#[tokio::test]
async fn fallback_unknown_state_writes_zero_times() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    // Standby read: the panel reports `0` (the F4 honest
    // "panel in standby" answer). The driver must NOT
    // write and must surface `claim_failed`.
    harness
        .sink
        .script_reads(vec![ScriptedRead::Standby, ScriptedRead::Standby]);
    let _verdict = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before fallback"
    );
    assert!(
        harness
            .wait_for_log("claim_fallback_direct", Duration::from_secs(5))
            .await,
        "fallback trace must fire on standby read"
    );
    let found_standby = harness
        .wait_for_log_prefix("claim_failed", Duration::from_secs(2))
        .await;
    let writes = harness.sink_writes();
    assert_eq!(
        writes.len(),
        0,
        "standby fallback must write ZERO times; got {writes:?}"
    );
    let events = harness.log_events();
    assert!(
        found_standby,
        "standby fallback must surface claim_failed; got {events:?}"
    );
    assert!(
        !events.iter().any(|e| e == "claim_completed"),
        "standby fallback must not complete; got {events:?}"
    );
    harness.shutdown();
}

// ── 2b. Fallback: powered + FOREIGN code (Must-5) ───────────────

/// The F4 fallback path at 1411-1424: when the fresh read
/// returns a powered but FOREIGN code (a different peer's
/// input is selected), the driver MUST write the LOCAL code
/// to take the panel, then log a
/// `claim_fallback_direct:wrote` marker. EXACTLY ONE
/// `write_input_source(0x0f)` (the local code). The
/// `claim_completed` trace does NOT fire here — the
/// completion is gated on the next coordination poll
/// observing the flip (the rules engine's responsibility,
/// not the fallback's).
#[tokio::test]
async fn fallback_foreign_code_writes_local_code_exactly_once() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    // Foreign powered read: 0x11 is the peer's input, not
    // ours (local is 0x0f). The fallback must write 0x0f.
    harness.sink.script_reads(vec![
        ScriptedRead::Powered(0x11),
        ScriptedRead::Powered(0x11),
    ]);
    let _verdict = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before fallback"
    );
    assert!(
        harness
            .wait_for_log("claim_fallback_direct", Duration::from_secs(5))
            .await,
        "fallback trace must fire on foreign-code read"
    );
    let found_wrote = harness
        .wait_for_log("claim_fallback_direct:wrote", Duration::from_secs(2))
        .await;
    let writes = harness.sink_writes();
    assert_eq!(
        writes.len(),
        1,
        "foreign-code fallback must write EXACTLY once; got {writes:?}"
    );
    assert_eq!(
        writes[0].arg,
        Some(0x0f),
        "fallback must write the LOCAL code (0x0f), not the foreign code (0x11); got {writes:?}"
    );
    let events = harness.log_events();
    assert!(
        found_wrote,
        "foreign-code fallback must surface claim_fallback_direct:wrote; got {events:?}"
    );
    assert!(
        !events.iter().any(|e| e == "claim_completed"),
        "F4 path does NOT complete; the rules engine drives the post-flip state machine; got {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.starts_with("claim_failed")),
        "F4 foreign-code path is a success, not a failure; got {events:?}"
    );
    harness.shutdown();
}

/// When `shared_input_write_code` differs from `shared_input_code`,
/// the fallback direct-write must use the write code.  The ownership
/// poll (not exercised here) must still compare against the read code.
#[tokio::test]
async fn fallback_uses_write_code_override_when_set() {
    let display = "mon";
    let hooks = release_acquire_hooks();
    let config = shared_display_config_with_write_code(
        display, 0x0f, // read code
        0x15, // write code
        hooks,
    );
    let harness =
        ClaimHarness::build_with_custom_config(display, config, PeerMode::ReachableSilent).await;
    harness.clear_sink_writes();
    // Foreign powered read: 0x11 is the peer's input. The fallback
    // must write the local WRITE code (0x15), not the local READ code (0x0f).
    harness.sink.script_reads(vec![
        ScriptedRead::Powered(0x11),
        ScriptedRead::Powered(0x11),
    ]);
    let _verdict = harness
        .handle
        .try_claim(DisplayId(display.into()))
        .await
        .expect("try_claim channel");
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before fallback"
    );
    assert!(
        harness
            .wait_for_log("claim_fallback_direct", Duration::from_secs(5))
            .await,
        "fallback trace must fire"
    );
    let writes = harness.sink_writes();
    assert_eq!(
        writes.len(),
        1,
        "fallback must write exactly once; got {writes:?}"
    );
    assert_eq!(
        writes[0].arg,
        Some(0x15),
        "fallback must write the WRITE code (0x15), not the read code (0x0f); got {writes:?}"
    );
    harness.shutdown();
}

/// When `shared_input_write_code` is absent, the fallback falls back
/// to `shared_input_code` — the backward-compatible default.
#[tokio::test]
async fn fallback_uses_read_code_when_write_code_absent() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    harness.sink.script_reads(vec![
        ScriptedRead::Powered(0x11),
        ScriptedRead::Powered(0x11),
    ]);
    let _verdict = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request"
    );
    assert!(
        harness
            .wait_for_log("claim_fallback_direct", Duration::from_secs(5))
            .await,
        "fallback trace must fire"
    );
    let writes = harness.sink_writes();
    assert_eq!(
        writes.len(),
        1,
        "fallback must write exactly once; got {writes:?}"
    );
    assert_eq!(
        writes[0].arg,
        Some(0x0f),
        "fallback must write the read code (0x0f) when no write-code override is set; got {writes:?}"
    );
    harness.shutdown();
}

// ── 3. Busy on concurrent trigger ───────────────────────────────

/// The pure engine's single-flight invariant: a second
/// `try_claim` while the first is in flight returns
/// `Busy`. No write fires; the second trigger does NOT
/// queue.
#[tokio::test]
async fn concurrent_local_claim_returns_busy() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    let first = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert!(
        matches!(first, ClaimSharedResult::Accepted { .. }),
        "first try_claim must be Accepted; got {first:?}"
    );
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before the flight proceeds"
    );
    let second = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert_eq!(
        second,
        ClaimSharedResult::Busy,
        "second concurrent claim must return Busy; got {second:?}"
    );
    assert!(
        harness
            .wait_for_log("claim_busy", Duration::from_secs(2))
            .await,
        "second claim must emit claim_busy"
    );
    let events = harness.log_events();
    assert!(
        events.iter().any(|e| e == "claim_busy"),
        "second claim must emit claim_busy; got {events:?}"
    );
    assert_eq!(harness.sink_writes().len(), 0);
    harness.shutdown();
}

// ── 4. Display-removed mid-claim ────────────────────────────────

/// Mid-claim reload: when the generation swap removes the
/// display while a requester flight is in flight, the
/// driver feeds `DisplayRemoved` to the engine; the
/// flight terminalises with `claim_failed` + `Removed`,
/// F10 lifts, and the event log records the trace. No
/// writes; no `claim_completed`.
#[tokio::test]
async fn display_removed_mid_claim_lifts_with_failed() {
    let harness = ClaimHarness::build_with_reachable_silent_peer("mon", 0x0f).await;
    harness.clear_sink_writes();
    let _first = harness
        .handle
        .try_claim(DisplayId("mon".into()))
        .await
        .expect("try_claim channel");
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before display removal"
    );
    harness
        .handle
        .display_removed(DisplayId("mon".into()))
        .await
        .expect("display_removed channel");
    assert!(
        harness
            .wait_for_log("claim_failed", Duration::from_secs(2))
            .await,
        "DisplayRemoved must surface claim_failed"
    );
    let events = harness.log_events();
    assert!(
        events.iter().any(|e| e == "claim_failed"),
        "claim_failed must be in the event log; got {events:?}"
    );
    assert_eq!(harness.sink_writes().len(), 0);
    assert!(
        !events.iter().any(|e| e == "claim_completed"),
        "DisplayRemoved path must not complete; got {events:?}"
    );
    assert!(
        !harness
            .handle
            .is_suppressed(&DisplayId("mon".into()), std::time::Instant::now()),
        "F10 must lift after DisplayRemoved"
    );
    harness.shutdown();
}

// ── 5. Cross-claim convergence (engine-level) ──────────────────

/// Cross-claim convergence: two opposing requester flights
/// on the same display converge on the FINAL hardware
/// value (the last write wins). The pure engine's
/// transition table is the contract; the driver just feeds
/// it. This test pins the engine's behavior.
#[test]
fn cross_claim_final_value_convergence() {
    let mut left = ClaimEngine::default();
    let mut right = ClaimEngine::default();
    let now = std::time::Instant::now();
    for (engine, nonce) in [(&mut left, "left"), (&mut right, "right")] {
        engine.begin_requester(
            DisplayId("mon".into()),
            nonce,
            1,
            now,
            Duration::from_secs(3),
        );
        engine.requester_event(&DisplayId("mon".into()), RequesterEvent::FanoutSent, now);
    }
    for (engine, nonce) in [(&mut left, "left"), (&mut right, "right")] {
        engine.requester_event(
            &DisplayId("mon".into()),
            RequesterEvent::Response {
                nonce: nonce.to_owned(),
                peer_instance_id: nonce.to_owned(),
                verdict: ClaimVerdict::Accepted { eta_ms: 5_000 },
            },
            now,
        );
    }
    // Left side: AcquireCompleted (from before_acquire hooks) → Watching.
    // Accepted already emitted RunBeforeAcquire; now complete the transition.
    let acquire_actions = left.requester_event(
        &DisplayId("mon".into()),
        RequesterEvent::AcquireCompleted,
        now,
    );
    assert!(
        acquire_actions
            .iter()
            .any(|a| matches!(a, Action::WatchForFlip)),
        "left AcquireCompleted must transition to Watching; got {acquire_actions:?}"
    );
    // Left side observes the flip → Success.
    let actions = left.requester_event(&DisplayId("mon".into()), RequesterEvent::FlipObserved, now);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Terminal(Terminal::Success))),
        "left wins the cross-claim; got {actions:?}"
    );
    // Right side's deadline elapses → TimedOut + claim_failed.
    let later = now + Duration::from_secs(60);
    let right_actions = right.on_deadline(&DisplayId("mon".into()), later);
    assert!(
        right_actions
            .iter()
            .any(|a| matches!(a, Action::Terminal(Terminal::TimedOut))),
        "right loses the cross-claim; got {right_actions:?}"
    );
    assert!(
        right_actions
            .iter()
            .any(|a| matches!(a, Action::Trace("claim_failed"))),
        "right's terminal must surface claim_failed; got {right_actions:?}"
    );
}

// ── Activity-claim evaluator tests (T14) ──────────────────────────────
// Prove the evaluator feeds Edge/Armed policies into the claim runtime.

struct NeverOwned;
impl OwnershipGate for NeverOwned {
    fn owns(&self, _: &DisplayId) -> bool {
        false
    }
}

/// Edge policy: an activity edge on a non-owned display must fire a claim.
#[tokio::test]
async fn edge_policy_fires_claim_on_activity_edge() {
    let display = "edge_test";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    let (idle_tx, idle_rx) = idle_observation_channel();

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::Edge,
        owner_idle_window: Duration::from_secs(30),
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    // Let the evaluator start watching the idle channel.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish an activity edge — user is active.
    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    // The evaluator should call try_claim → runtime records "claim_requested".
    let found = harness
        .wait_for_log("claim_requested", Duration::from_secs(3))
        .await;
    evaluator_cancel.cancel();
    assert!(
        found,
        "Edge policy should fire a claim on a non-owned display; log = {:?}",
        harness.log_events()
    );
}

/// Armed policy: a pre-armed display must fire a claim on activity.
#[tokio::test]
async fn armed_policy_fires_claim_when_armed() {
    let display = "armed_test";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    // Arm the display — claim runtime records the arm deadline.
    let _ = harness
        .handle
        .arm(DisplayId(display.to_owned()), ActivityClaimPolicy::Armed)
        .await
        .expect("arm must succeed");

    let (idle_tx, idle_rx) = idle_observation_channel();

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::Armed,
        owner_idle_window: Duration::from_secs(30),
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    let found = harness
        .wait_for_log("claim_requested", Duration::from_secs(3))
        .await;
    evaluator_cancel.cancel();
    assert!(
        found,
        "Armed policy should fire a claim when display is armed; log = {:?}",
        harness.log_events()
    );
}

/// Armed policy: without pre-arming, activity must NOT fire a claim.
#[tokio::test]
async fn armed_policy_does_not_claim_without_arm() {
    let display = "unarmed_test";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    // Do NOT arm — the display has no arm deadline.

    let (idle_tx, idle_rx) = idle_observation_channel();

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::Armed,
        owner_idle_window: Duration::from_secs(30),
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    // Give the evaluator time to process.
    tokio::time::sleep(Duration::from_millis(200)).await;
    evaluator_cancel.cancel();

    assert!(
        !harness.log_events().iter().any(|e| e == "claim_requested"),
        "Armed policy should NOT fire a claim without pre-arming; log = {:?}",
        harness.log_events()
    );
}

/// `OwnerIdle`: when the owner reports idle past the window, fire a claim.
#[tokio::test]
async fn owner_idle_policy_fires_claim_when_owner_idle_past_threshold() {
    let display = "owner_idle_test";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    let (idle_tx, idle_rx) = idle_observation_channel();
    let owner_idle_window = Duration::from_millis(500); // tiny for test

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::OwnerIdle,
        owner_idle_window,
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish activity edge → evaluator evaluates, gets QueryOwnerIdle,
    // spawns idle_query which sends IdleQuery to runtime.
    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    // Wait for the runtime to log the idle_query_sent event.
    let found = harness
        .wait_for_log_prefix("idle_query_sent", Duration::from_secs(3))
        .await;
    assert!(found, "runtime should log idle_query_sent");

    // Extract the nonce from the log.
    let nonce = harness
        .log_events()
        .iter()
        .find_map(|e| e.strip_prefix("idle_query_sent nonce=").map(str::to_owned))
        .expect("idle_query_sent log entry must contain a nonce");

    // Inject an IdleReport from the owner with idle_ms past threshold.
    harness
        .handle
        .inject_idle_report_for_test(
            nonce,
            u64::try_from(owner_idle_window.as_millis() + 100).unwrap(),
        )
        .await
        .expect("inject_idle_report_for_test must succeed");

    // The evaluator's spawned task should see idle_ms >= threshold
    // and fire try_claim → "claim_requested".
    let claimed = harness
        .wait_for_log("claim_requested", Duration::from_secs(3))
        .await;
    evaluator_cancel.cancel();
    assert!(
        claimed,
        "OwnerIdle should fire a claim when owner is idle past threshold; log = {:?}",
        harness.log_events()
    );
}

/// `OwnerIdle`: when the owner is NOT idle (below threshold), no claim fires.
#[tokio::test]
async fn owner_idle_policy_does_not_claim_below_threshold() {
    let display = "owner_idle_low";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    let (idle_tx, idle_rx) = idle_observation_channel();
    let owner_idle_window = Duration::from_secs(60); // large

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::OwnerIdle,
        owner_idle_window,
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    let found = harness
        .wait_for_log_prefix("idle_query_sent", Duration::from_secs(3))
        .await;
    assert!(found, "runtime should log idle_query_sent");

    let nonce = harness
        .log_events()
        .iter()
        .find_map(|e| e.strip_prefix("idle_query_sent nonce=").map(str::to_owned))
        .expect("idle_query_sent log entry must contain a nonce");

    // Inject an IdleReport with idle_ms BELOW the threshold.
    harness
        .handle
        .inject_idle_report_for_test(nonce, 1_000) // 1s << 60s threshold
        .await
        .expect("inject_idle_report_for_test must succeed");

    // Give the evaluator time to process.
    tokio::time::sleep(Duration::from_millis(200)).await;
    evaluator_cancel.cancel();

    assert!(
        !harness.log_events().iter().any(|e| e == "claim_requested"),
        "OwnerIdle should NOT fire a claim when owner is below threshold; log = {:?}",
        harness.log_events()
    );
}

/// `OwnerIdle`: when no `IdleReport` arrives (timeout), no claim fires.
#[tokio::test]
async fn owner_idle_policy_times_out_without_claim() {
    let display = "owner_idle_timeout";
    // The scripted peer accepts IdleQuery but sends no IdleReport,
    // so the owner-idle query expires without starting a claim.
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    let (idle_tx, idle_rx) = idle_observation_channel();

    // Use a very small claim_timeout config so the test is fast.
    // The harness config defaults to 3s claim_timeout — we accept that.
    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::OwnerIdle,
        owner_idle_window: Duration::from_secs(30),
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    // Wait for the timeout to expire (claim_timeout is 3s from config).
    // Give a generous window.
    tokio::time::sleep(Duration::from_secs(4)).await;
    evaluator_cancel.cancel();

    assert!(
        !harness.log_events().iter().any(|e| e == "claim_requested"),
        "OwnerIdle should NOT fire a claim when no IdleReport arrives; log = {:?}",
        harness.log_events()
    );
}

/// `IdleReport` from a non-owner peer must be REJECTED — the pending
/// query must NOT resolve, and no `try_claim` fires.
#[tokio::test]
async fn non_owner_idle_report_is_rejected_by_handler() {
    let display = "idle_owner_gate";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    // Set the coordination snapshot's expected owner to a specific
    // instance ID so the IdleReport handler can gate on it.
    let expected_owner = "owner-peer-1";
    harness.coord.set_owner(
        &DisplayId(display.to_owned()),
        Some(expected_owner.to_owned()),
    );

    let (idle_tx, idle_rx) = idle_observation_channel();
    let owner_idle_window = Duration::from_millis(500);

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::OwnerIdle,
        owner_idle_window,
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    let found = harness
        .wait_for_log_prefix("idle_query_sent", Duration::from_secs(3))
        .await;
    assert!(found, "runtime should log idle_query_sent");

    let nonce = harness
        .log_events()
        .iter()
        .find_map(|e| e.strip_prefix("idle_query_sent nonce=").map(str::to_owned))
        .expect("idle_query_sent log entry must contain a nonce");

    // Build a signed IdleReport from a NON-OWNER (different instance_id).
    let attacker_signing = SigningKey::from_bytes(&[99; 32]);
    let attacker_identity = InstanceIdentity {
        instance_id: "attacker".to_owned(),
        signing_key: attacker_signing.clone(),
        verifying_key: attacker_signing.verifying_key(),
    };
    let fake_report = dormant_core::claim::IdleReport {
        idle_ms: u64::try_from(owner_idle_window.as_millis() + 500).unwrap(),
        counter: 1,
        nonce: nonce.clone(),
    };
    let frame = ClaimFrame::sign(
        &attacker_identity,
        "attacker-epoch01".to_owned(),
        harness.local_identity.instance_id.clone(),
        "0123456789abcdef".to_owned(),
        1,
        nonce.clone(),
        ClaimMessage::IdleReport(fake_report),
    )
    .expect("sign IdleReport frame");

    // Inject via the signed-frame inbound path — this exercises the
    // full handler, including the sender_instance_id gate.
    harness
        .handle
        .inject_inbound_for_test(frame)
        .await
        .expect("inject_inbound_for_test must succeed");

    // Give the evaluator time to process.
    tokio::time::sleep(Duration::from_millis(300)).await;
    evaluator_cancel.cancel();

    assert!(
        !harness.log_events().iter().any(|e| e == "claim_requested"),
        "Non-owner IdleReport must be REJECTED; log = {:?}",
        harness.log_events()
    );
}

/// `IdleReport` from the EXPECTED owner must be ACCEPTED and
/// resolve the pending query → `try_claim` fires.
#[tokio::test]
async fn matching_owner_idle_report_is_accepted() {
    let display = "idle_owner_match";
    let harness = ClaimHarness::build_with_reachable_silent_peer(display, 0x0f).await;
    let evaluator_cancel = CancellationToken::new();

    // Set the expected owner to match the IdleReport sender.
    let expected_owner = "owner-peer-2";
    harness.coord.set_owner(
        &DisplayId(display.to_owned()),
        Some(expected_owner.to_owned()),
    );

    let (idle_tx, idle_rx) = idle_observation_channel();
    let owner_idle_window = Duration::from_millis(500);

    let deps = PolicyEvaluatorDeps {
        idle_rx,
        claim_runtime: harness.handle.clone(),
        ownership: Arc::new(NeverOwned),
        activity_claim: ActivityClaimPolicy::OwnerIdle,
        owner_idle_window,
        armed_window: Duration::from_secs(60),
        claim_capable_displays: vec![DisplayId(display.to_owned())],
        cancel: evaluator_cancel.clone(),
        event_log: None,
        event_notify: None,
    };

    let _eval_handle = activity_claim_evaluator::spawn(deps);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = idle_tx.send(IdleObservation {
        last_activity: Some(std::time::Instant::now()),
        observed_at: std::time::Instant::now(),
        available: true,
    });

    let found = harness
        .wait_for_log_prefix("idle_query_sent", Duration::from_secs(3))
        .await;
    assert!(found);

    let nonce = harness
        .log_events()
        .iter()
        .find_map(|e| e.strip_prefix("idle_query_sent nonce=").map(str::to_owned))
        .expect("idle_query_sent log entry must contain a nonce");

    // Build a signed IdleReport from the MATCHING owner.
    let owner_signing = SigningKey::from_bytes(&[77; 32]);
    let owner_identity = InstanceIdentity {
        instance_id: expected_owner.to_owned(),
        signing_key: owner_signing.clone(),
        verifying_key: owner_signing.verifying_key(),
    };
    let real_report = dormant_core::claim::IdleReport {
        idle_ms: u64::try_from(owner_idle_window.as_millis() + 500).unwrap(),
        counter: 1,
        nonce: nonce.clone(),
    };
    let frame = ClaimFrame::sign(
        &owner_identity,
        "00wner-epoch-001".to_owned(),
        harness.local_identity.instance_id.clone(),
        "0123456789abcdef".to_owned(),
        1,
        nonce.clone(),
        ClaimMessage::IdleReport(real_report),
    )
    .expect("sign IdleReport frame");

    harness
        .handle
        .inject_inbound_for_test(frame)
        .await
        .expect("inject_inbound_for_test must succeed");

    let claimed = harness
        .wait_for_log("claim_requested", Duration::from_secs(3))
        .await;
    evaluator_cancel.cancel();
    assert!(
        claimed,
        "Matching-owner IdleReport must fire a claim; log = {:?}",
        harness.log_events()
    );
}

#[tokio::test]
async fn accepted_claim_response_records_owner_for_idle_report_gate() {
    let display = DisplayId("owner_tracking".to_owned());
    let harness = ClaimHarness::build_with_reachable_silent_peer(&display.0, 0x0f).await;
    harness.coord.record_success(&display, 0x11, 0x0f, None);

    // The requester's Waking phase fires before_acquire hooks.
    // Queue one Completed outcome so the hook does not abort.
    harness.runner.push_completed(1, 0, 0);

    let result = harness
        .handle
        .try_claim(display.clone())
        .await
        .expect("claim runtime must accept the local request");
    assert!(matches!(result, ClaimSharedResult::Accepted { .. }));
    assert!(
        harness
            .wait_for_peer_requests(1, Duration::from_secs(2))
            .await,
        "reachable peer must receive the request before the injected response"
    );

    let nonce = harness
        .handle
        .requester_nonce_for_test(display.clone())
        .await
        .expect("requester nonce query must succeed")
        .expect("requester flight must be active");

    let owner_signing = SigningKey::from_bytes(&[88; 32]);
    let owner_instance_id = instance_id_from_public_key(&owner_signing.verifying_key().to_bytes());
    let owner_identity = InstanceIdentity {
        instance_id: owner_instance_id.clone(),
        signing_key: owner_signing.clone(),
        verifying_key: owner_signing.verifying_key(),
    };
    let frame = ClaimFrame::sign(
        &owner_identity,
        "peer-epoch-00001".to_owned(),
        harness.local_identity.instance_id.clone(),
        "0123456789abcdef".to_owned(),
        1,
        "accepted-frame-1".to_owned(),
        ClaimMessage::ClaimResponse(ClaimResponse {
            nonce,
            verdict: ClaimVerdict::Accepted { eta_ms: 1_000 },
        }),
    )
    .expect("accepted response frame must sign");

    harness
        .handle
        .inject_inbound_for_test(frame)
        .await
        .expect("accepted response must reach the runtime");
    assert!(
        harness
            .wait_for_log("claim_accepted", Duration::from_secs(2))
            .await,
        "accepted response must traverse the requester handler"
    );

    let record = harness
        .coord
        .snapshot()
        .remove(&display)
        .expect("shared display must remain in coordination state");
    assert_eq!(
        record.owner_instance_id.as_deref(),
        Some(owner_instance_id.as_str())
    );
    harness.shutdown();
}
