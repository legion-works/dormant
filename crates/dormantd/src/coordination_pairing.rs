//! Confirmed SPAKE2 pairing sessions for dormant instances.

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use dormant_core::peers::{
    DiscoverAnnounce, MAX_PAIR_FRAME_BYTES, PAIR_PROTOCOL_VERSION, PairError, PairFrame, PairRole,
    PeerRecord, build_pairing_transcript, instance_id_from_public_key, load_or_create_identity,
    upsert_peer,
};
use ed25519_dalek::VerifyingKey;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::coordination_mdns::{MdnsBackend, PairDiscovery, resolve_bind_ip};

#[allow(
    dead_code,
    reason = "Task 14's TCP adapter invokes the SPAKE2 state machine."
)]
const PAIRING_CONTEXT: &[u8] = b"dormant-pairing-v2";
#[allow(
    dead_code,
    reason = "Task 14's TCP adapter invokes the SPAKE2 state machine."
)]
const MAX_PAIR_ATTEMPTS: usize = 10;
const NONCE_BYTES: usize = 32;
#[allow(
    dead_code,
    reason = "Task 14's TCP adapter invokes the SPAKE2 state machine."
)]
const MAC_BYTES: usize = 32;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[allow(
    dead_code,
    reason = "Task 14's TCP adapter invokes the SPAKE2 state machine."
)]
type HmacSha256 = Hmac<Sha256>;

/// Local failures that are not part of the ratified pairing wire protocol.
#[derive(Debug)]
pub enum PairSessionError {
    /// A protocol-level failure reported by the peer or detected locally.
    Wire(PairError),
    /// The selected discovery record identifies this daemon.
    SelfPair,
    /// A session is already active for this pairing window.
    #[allow(
        dead_code,
        reason = "Task 14 acquires this single-flight guard around TCP sessions."
    )]
    Busy,
    /// Pairing is disabled by runtime configuration.
    Disabled,
    /// The requested pairing window no longer exists.
    UnknownPair,
    /// Local persistence or entropy failure.
    Local(String),
}

impl std::fmt::Display for PairSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(error) => write!(formatter, "pairing rejected: {error:?}"),
            Self::SelfPair => formatter.write_str("cannot pair an instance with itself"),
            Self::Busy => formatter.write_str("a pairing session is already active"),
            Self::Disabled => formatter.write_str("coordination pairing is disabled"),
            Self::UnknownPair => formatter.write_str("pairing window not found"),
            Self::Local(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for PairSessionError {}

/// Public, non-secret pairing-window state for IPC and web surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingState {
    /// The responder window is accepting one session.
    Pairing,
    /// Both peers completed confirmation and persisted each other.
    #[allow(
        dead_code,
        reason = "Task 14 sets this after transport confirmation completes."
    )]
    Paired,
    /// The operator cancelled the window.
    Cancelled,
    /// The window elapsed before a successful confirmation.
    #[allow(
        dead_code,
        reason = "Task 14 observes expiry while accepting TCP sessions."
    )]
    Timeout,
    /// A session ended without producing a peer record.
    #[allow(dead_code, reason = "Task 14 exposes failed responder-session status.")]
    Error,
}

/// Public pairing-window status. It intentionally contains no code or key material.
#[derive(Debug, Clone)]
pub(crate) struct PairingStatus {
    /// Opaque local identifier for this pairing window.
    pub pair_id: String,
    /// Current lifecycle state.
    pub state: PairingState,
    /// Paired peer identity once the session succeeds.
    pub peer_instance_id: Option<String>,
}

/// A responder window newly opened for the local operator.
pub(crate) struct OpenPairing {
    /// Public pairing-window identifier.
    pub pair_id: String,
    /// Pairing code returned once in the loopback-only open response.
    pub code: String,
    /// RFC 3339 UTC deadline returned with the code.
    pub expires_at: String,
}

#[allow(
    dead_code,
    reason = "Task 14 constructs local peers for the TCP handshake adapter."
)]
pub(crate) struct LocalPeer {
    state_dir: PathBuf,
    display_name: String,
    identity: dormant_core::peers::InstanceIdentity,
}

#[allow(
    dead_code,
    reason = "Task 14 constructs local peers for the TCP handshake adapter."
)]
impl LocalPeer {
    fn load(state_dir: PathBuf, display_name: String) -> Result<Self, PairSessionError> {
        let identity = load_or_create_identity(&state_dir)
            .map_err(|error| PairSessionError::Local(error.to_string()))?;
        Ok(Self {
            state_dir,
            display_name,
            identity,
        })
    }

    fn public_key(&self) -> [u8; 32] {
        self.identity.verifying_key.to_bytes()
    }

    fn persist(&self, peer: &PeerIdentity) -> Result<(), PairSessionError> {
        let paired_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| PairSessionError::Local(error.to_string()))?;
        let record = PeerRecord {
            instance_id: peer.instance_id.clone(),
            ed25519_pub: base64::engine::general_purpose::STANDARD.encode(peer.public_key),
            display_name: peer.display_name.clone(),
            paired_at,
        };
        upsert_peer(&self.state_dir.join("peers.json"), record)
            .map_err(|error| PairSessionError::Local(error.to_string()))
    }
}

#[derive(Clone)]
#[allow(
    dead_code,
    reason = "Task 14 passes authenticated peer identities through the frame state machine."
)]
struct PeerIdentity {
    instance_id: String,
    display_name: String,
    public_key: [u8; 32],
    nonce: [u8; NONCE_BYTES],
}

/// A bounded responder pairing window.
#[allow(
    dead_code,
    reason = "Task 14 mutates these session bounds while accepting TCP frames."
)]
pub(crate) struct PairingWindow {
    pair_id: String,
    display_name: String,
    code: Zeroizing<Vec<u8>>,
    expires_at: Instant,
    attempts: usize,
    seen_initiator_nonces: HashSet<[u8; NONCE_BYTES]>,
    cancelled: bool,
    completed: bool,
    in_flight: Arc<AtomicBool>,
    state: PairingState,
    peer_instance_id: Option<String>,
}

/// Daemon-lifetime owner of responder windows and non-secret status.
pub(crate) struct PairingManager {
    state_dir: PathBuf,
    identity: Option<dormant_core::peers::InstanceIdentity>,
    enabled: bool,
    pairing_window: Duration,
    windows: Mutex<HashMap<String, PairingWindow>>,
}

impl PairingManager {
    /// Construct the runtime pairing manager. It opens no listener by itself.
    pub(crate) fn new(
        state_dir: &Path,
        enabled: bool,
        pairing_window: Duration,
    ) -> Result<Self, PairSessionError> {
        let identity = enabled
            .then(|| load_or_create_identity(state_dir))
            .transpose()
            .map_err(|error| PairSessionError::Local(error.to_string()))?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            identity,
            enabled,
            pairing_window,
            windows: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn local_peer(&self, display_name: String) -> Result<LocalPeer, PairSessionError> {
        LocalPeer::load(self.state_dir.clone(), display_name)
    }

    /// Open one responder window. Transport advertisement is attached by Task 14.
    pub(crate) fn open(&self, display_name: String) -> Result<OpenPairing, PairSessionError> {
        if !self.enabled {
            return Err(PairSessionError::Disabled);
        }
        let (window, open) = PairingWindow::open_named(self.pairing_window, display_name)?;
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(open.pair_id.clone(), window);
        Ok(open)
    }

    /// Reject local self-pair attempts before transport receives any secret input.
    pub(crate) fn join_preflight(&self, instance_id: &str) -> Result<(), PairSessionError> {
        if !self.enabled {
            return Err(PairSessionError::Disabled);
        }
        if self
            .identity
            .as_ref()
            .is_some_and(|identity| identity.instance_id == instance_id)
        {
            return Err(PairSessionError::SelfPair);
        }
        decode_base64url_key(instance_id)?;
        Ok(())
    }

    /// Return public status without exposing code or key material.
    pub(crate) fn status(&self, pair_id: &str) -> Result<PairingStatus, PairSessionError> {
        if !self.enabled {
            return Err(PairSessionError::Disabled);
        }
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pair_id)
            .map(PairingWindow::status)
            .ok_or(PairSessionError::UnknownPair)
    }

    /// Cancel a responder window and return its public terminal status.
    pub(crate) fn cancel(&self, pair_id: &str) -> Result<PairingStatus, PairSessionError> {
        if !self.enabled {
            return Err(PairSessionError::Disabled);
        }
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = windows
            .get_mut(pair_id)
            .ok_or(PairSessionError::UnknownPair)?;
        window.cancel();
        Ok(window.status())
    }
}

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CONCURRENT_PAIRING_CONNS: usize = 8;
// The attempt cap burns code guesses; this separate cap bounds clients that never send a hello.
const MAX_PAIRING_CONNS_PER_WINDOW: usize = 30;

/// Owns short-lived TCP listeners and their matching mDNS advertisements.
pub(crate) struct PairingTransport<B: MdnsBackend + 'static> {
    manager: Arc<PairingManager>,
    discovery: Arc<Mutex<PairDiscovery<B>>>,
    pairing_port: u16,
    bind_override: Option<String>,
    test_bind_ip: Option<IpAddr>,
    cancel: tokio_util::sync::CancellationToken,
    windows: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
}

impl<B: MdnsBackend + 'static> PairingTransport<B> {
    /// Construct a transport. It opens no socket until the operator opens a window.
    pub(crate) fn new(
        manager: Arc<PairingManager>,
        discovery: PairDiscovery<B>,
        pairing_port: u16,
        bind_override: Option<String>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            manager,
            discovery: Arc::new(Mutex::new(discovery)),
            pairing_port,
            bind_override,
            test_bind_ip: None,
            cancel,
            windows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    fn new_with_bind_ip_for_test(
        manager: Arc<PairingManager>,
        discovery: PairDiscovery<B>,
        pairing_port: u16,
        bind_ip: IpAddr,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            manager,
            discovery: Arc::new(Mutex::new(discovery)),
            pairing_port,
            bind_override: None,
            test_bind_ip: Some(bind_ip),
            cancel,
            windows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Open a bound-and-advertised responder window.
    pub(crate) async fn open(&self, display_name: String) -> Result<OpenPairing, PairSessionError> {
        if !self.manager.enabled {
            return Err(PairSessionError::Disabled);
        }
        if !self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            return Err(PairSessionError::Busy);
        }
        let bind_ip = match self.test_bind_ip {
            Some(ip) => ip,
            None => resolve_bind_ip(self.bind_override.as_deref()).map_err(|error| {
                PairSessionError::Local(format!("resolve pairing bind address: {error}"))
            })?,
        };
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(bind_ip, self.pairing_port))
            .await
            .map_err(|error| {
                PairSessionError::Local(format!("bind pairing listener at {bind_ip}: {error}"))
            })?;
        let actual = listener.local_addr().map_err(|error| {
            PairSessionError::Local(format!("read pairing listener address: {error}"))
        })?;
        let open = self.manager.open(display_name.clone())?;
        let identity = self
            .manager
            .identity
            .as_ref()
            .ok_or(PairSessionError::Disabled)?;
        let announce = DiscoverAnnounce {
            protocol_version: PAIR_PROTOCOL_VERSION,
            instance_id: identity.instance_id.clone(),
            display_name,
            pairing_port: actual.port(),
            window_id: open.pair_id.clone(),
        };
        if let Err(error) = self
            .discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open_pairing_window(announce)
        {
            let _ = self.manager.cancel(&open.pair_id);
            return Err(PairSessionError::Local(format!(
                "advertise pairing window: {error}"
            )));
        }
        tracing::info!(event = "pairing_listener_bound", bind = %actual);

        let window_cancel = self.cancel.child_token();
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(open.pair_id.clone(), window_cancel.clone());
        tokio::spawn(run_listener(
            listener,
            Arc::clone(&self.manager),
            Arc::clone(&self.discovery),
            Arc::clone(&self.windows),
            open.pair_id.clone(),
            window_cancel,
        ));
        Ok(open)
    }

    /// Join a discovered responder using its advertised window and address.
    pub(crate) async fn join(
        &self,
        display_name: String,
        target_instance_id: String,
        code: String,
    ) -> Result<(), PairSessionError> {
        self.manager.join_preflight(&target_instance_id)?;
        let (address, window_id) = {
            let mut discovery = self
                .discovery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            discovery.start_browse().map_err(|error| {
                PairSessionError::Local(format!("start pairing browse: {error}"))
            })?;
            discovery.drain_browse().map_err(|error| {
                PairSessionError::Local(format!("drain pairing browse: {error}"))
            })?;
            let address = discovery
                .discovered_peer_addr(&target_instance_id)
                .ok_or_else(|| PairSessionError::Local("peer not discovered".to_owned()))?;
            let window_id = discovery
                .discovered_peers()
                .get(&target_instance_id)
                .map(|peer| peer.window_id.clone())
                .ok_or_else(|| PairSessionError::Local("peer not discovered".to_owned()))?;
            (address, window_id)
        };
        let mut stream =
            tokio::time::timeout(CONNECTION_TIMEOUT, tokio::net::TcpStream::connect(address))
                .await
                .map_err(|_| PairSessionError::Local("pairing connection timed out".to_owned()))?
                .map_err(|error| {
                    PairSessionError::Local(format!("connect pairing peer: {error}"))
                })?;
        let local = self.manager.local_peer(display_name)?;
        tokio::time::timeout(
            CONNECTION_TIMEOUT,
            run_initiator_over_stream(
                &mut stream,
                &local,
                target_instance_id,
                window_id,
                code.as_bytes(),
            ),
        )
        .await
        .map_err(|_| PairSessionError::Local("pairing session timed out".to_owned()))?
    }

    /// Cancel a local responder window and close its listener immediately.
    pub(crate) fn cancel(&self, pair_id: &str) -> Result<PairingStatus, PairSessionError> {
        let status = self.manager.cancel(pair_id)?;
        if let Some(cancel) = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pair_id)
        {
            cancel.cancel();
        }
        Ok(status)
    }
}

async fn run_listener<B: MdnsBackend + 'static>(
    listener: tokio::net::TcpListener,
    manager: Arc<PairingManager>,
    discovery: Arc<Mutex<PairDiscovery<B>>>,
    windows: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    pair_id: String,
    cancel: tokio_util::sync::CancellationToken,
) {
    use tokio::sync::{Semaphore, mpsc};
    use tokio::task::JoinSet;

    let deadline = manager
        .windows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&pair_id)
        .map_or_else(Instant::now, |window| window.expires_at);
    let deadline = tokio::time::Instant::from_std(deadline);
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_PAIRING_CONNS));
    let (success_tx, mut success_rx) = mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    let mut connections = 0_usize;
    loop {
        if connections >= MAX_PAIRING_CONNS_PER_WINDOW {
            break;
        }
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep_until(deadline) => {
                if let Ok(mut windows) = manager.windows.lock()
                    && let Some(window) = windows.get_mut(&pair_id)
                    && !window.completed && !window.cancelled {
                    window.state = PairingState::Timeout;
                }
                break;
            }
            Some(()) = success_rx.recv() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            accepted = listener.accept() => {
                let Ok((mut stream, _)) = accepted else { break; };
                connections += 1;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    continue;
                };
                let manager = Arc::clone(&manager);
                let pair_id = pair_id.clone();
                let cancel = cancel.clone();
                let success_tx = success_tx.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let result = tokio::select! {
                        () = cancel.cancelled() => Err(PairSessionError::Wire(PairError::Cancelled)),
                        result = tokio::time::timeout(
                            CONNECTION_TIMEOUT,
                            run_managed_responder(&mut stream, manager, &pair_id),
                        ) => match result {
                            Ok(result) => result,
                            Err(_) => Err(PairSessionError::Local("pairing connection timed out".to_owned())),
                        },
                    };
                    if result.is_ok() {
                        let _ = success_tx.send(());
                    }
                });
            }
        }
    }
    cancel.cancel();
    while tasks.join_next().await.is_some() {}
    discovery
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .close_pairing_window();
    windows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&pair_id);
}

async fn run_managed_responder<S>(
    stream: &mut S,
    manager: Arc<PairingManager>,
    pair_id: &str,
) -> Result<(), PairSessionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let hello = read_pair_frame(stream).await?;
    let (session, responder, remote, _flight) = {
        let mut windows = manager
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = windows
            .get_mut(pair_id)
            .ok_or(PairSessionError::UnknownPair)?;
        let flight = window.begin()?;
        let responder = manager.local_peer(window.display_name.clone())?;
        let remote = accept_hello(window, &responder, hello)?;
        let session = PairingWindow {
            pair_id: window.pair_id.clone(),
            display_name: window.display_name.clone(),
            code: window.code.clone(),
            expires_at: window.expires_at,
            attempts: 0,
            seen_initiator_nonces: HashSet::new(),
            cancelled: false,
            completed: false,
            in_flight: Arc::new(AtomicBool::new(false)),
            state: PairingState::Pairing,
            peer_instance_id: None,
        };
        (session, responder, remote, flight)
    };
    let (mut state, start) =
        responder_receive_msg1(&session, &responder, remote, read_pair_frame(stream).await?)?;
    for frame in &start {
        write_pair_frame(stream, &wire_to_frame(frame)?).await?;
    }
    let initiator_confirmation = [
        frame_for_wire(&read_pair_frame(stream).await?),
        frame_for_wire(&read_pair_frame(stream).await?),
    ];
    let confirmation = responder_receive_initiator(&mut state, &initiator_confirmation)?;
    write_pair_frame(stream, &wire_to_frame(&confirmation[0])?).await?;
    let terminal = frame_for_wire(&read_pair_frame(stream).await?);
    let result = responder_receive_result(&terminal, &state)?;
    write_pair_frame(stream, &wire_to_frame(&result)?).await?;
    let mut windows = manager
        .windows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let window = windows
        .get_mut(pair_id)
        .ok_or(PairSessionError::UnknownPair)?;
    if window.cancelled || window.completed || Instant::now() >= window.expires_at {
        return Err(PairSessionError::Wire(PairError::Cancelled));
    }
    // Reserve completion before the durable write so another completed task cannot persist later.
    window.completed = true;
    drop(windows);
    if let Err(error) = responder.persist(&state.remote) {
        if let Ok(mut windows) = manager.windows.lock()
            && let Some(window) = windows.get_mut(pair_id)
        {
            window.state = PairingState::Error;
        }
        return Err(error);
    }
    let mut windows = manager
        .windows
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let window = windows
        .get_mut(pair_id)
        .ok_or(PairSessionError::UnknownPair)?;
    window.state = PairingState::Paired;
    window.peer_instance_id = Some(state.remote.instance_id);
    tracing::info!(event = "pairing_confirmed");
    Ok(())
}

/// Drive the initiator half of the ratified pairing state machine over one stream.
///
/// Nothing is persisted until both transcript confirmations and the responder's
/// terminal result have been verified.
pub(crate) async fn run_initiator_over_stream<S>(
    stream: &mut S,
    local: &LocalPeer,
    remote_instance_id: String,
    window_id: String,
    code: &[u8],
) -> Result<(), PairSessionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut state, start) = begin_initiator(local, remote_instance_id, window_id, code)?;
    for frame in &start {
        write_pair_frame(stream, &wire_to_frame(frame)?).await?;
    }

    let responder_start = [
        frame_for_wire(&read_pair_frame(stream).await?),
        frame_for_wire(&read_pair_frame(stream).await?),
        frame_for_wire(&read_pair_frame(stream).await?),
    ];
    let confirmation = match initiator_receive_responder(&mut state, &responder_start) {
        Ok(confirmation) => confirmation,
        Err(error) => {
            write_rejection(stream, &error).await;
            return Err(error);
        }
    };
    for frame in &confirmation {
        write_pair_frame(stream, &wire_to_frame(frame)?).await?;
    }

    let responder_confirmation = frame_for_wire(&read_pair_frame(stream).await?);
    let result = match initiator_receive_confirmation(&state, &responder_confirmation) {
        Ok(result) => result,
        Err(error) => {
            write_rejection(stream, &error).await;
            return Err(error);
        }
    };
    write_pair_frame(stream, &wire_to_frame(&result)?).await?;

    let PairFrame::PairResult {
        accepted: true,
        error: None,
    } = read_pair_frame(stream).await?
    else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };

    let remote = PeerIdentity {
        instance_id: state.remote_instance_id.clone(),
        display_name: state
            .remote_display_name
            .clone()
            .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?,
        public_key: state
            .remote_public_key
            .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?,
        nonce: state
            .remote_nonce
            .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?,
    };
    local.persist(&remote)
}

/// Drive the responder half of the ratified pairing state machine over one stream.
///
/// The caller owns the window for the session so its attempt and single-flight
/// guards remain authoritative across reconnects.
#[allow(
    dead_code,
    reason = "The direct stream driver is exercised by loopback transport tests."
)]
pub(crate) async fn run_responder_over_stream<S>(
    stream: &mut S,
    window: &mut PairingWindow,
    responder: &LocalPeer,
) -> Result<(), PairSessionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _flight = window.begin()?;
    let remote = accept_hello(window, responder, read_pair_frame(stream).await?)?;
    let (mut state, start) =
        match responder_receive_msg1(window, responder, remote, read_pair_frame(stream).await?) {
            Ok(start) => start,
            Err(error) => {
                write_rejection(stream, &error).await;
                return Err(error);
            }
        };
    for frame in &start {
        write_pair_frame(stream, &wire_to_frame(frame)?).await?;
    }

    let initiator_confirmation = [
        frame_for_wire(&read_pair_frame(stream).await?),
        frame_for_wire(&read_pair_frame(stream).await?),
    ];
    let confirmation = responder_receive_initiator(&mut state, &initiator_confirmation)?;
    write_pair_frame(stream, &wire_to_frame(&confirmation[0])?).await?;

    let terminal = frame_for_wire(&read_pair_frame(stream).await?);
    let result = responder_receive_result(&terminal, &state)?;
    write_pair_frame(stream, &wire_to_frame(&result)?).await?;

    responder.persist(&state.remote)?;
    window.completed = true;
    window.state = PairingState::Paired;
    window.peer_instance_id = Some(state.remote.instance_id.clone());
    tracing::info!(event = "pairing_confirmed");
    Ok(())
}

async fn write_rejection<S>(stream: &mut S, error: &PairSessionError)
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let wire_error = match error {
        PairSessionError::Wire(error) => *error,
        _ => PairError::InvalidFrame,
    };
    let _ = write_pair_frame(
        stream,
        &PairFrame::PairResult {
            accepted: false,
            error: Some(wire_error),
        },
    )
    .await;
}

/// Pair two isolated state directories through a real loopback TCP listener.
///
/// This is an integration-test seam: production windows always resolve a
/// non-loopback LAN address before binding.
///
/// # Errors
///
/// Returns an error when either identity, TCP operation, pairing frame, or
/// durable peer write fails.
#[cfg(feature = "test-util")]
pub async fn pair_over_loopback_for_test(
    initiator_state_dir: PathBuf,
    responder_state_dir: PathBuf,
) -> Result<(), PairSessionError> {
    let initiator = LocalPeer::load(initiator_state_dir, "Initiator".to_owned())?;
    let responder = LocalPeer::load(responder_state_dir, "Responder".to_owned())?;
    let (mut window, open) =
        PairingWindow::open_named(Duration::from_secs(2), "Responder".to_owned())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| {
            PairSessionError::Local(format!("bind loopback pairing listener: {error}"))
        })?;
    let address = listener.local_addr().map_err(|error| {
        PairSessionError::Local(format!("read loopback pairing listener: {error}"))
    })?;
    let (connected, accepted) =
        tokio::join!(tokio::net::TcpStream::connect(address), listener.accept());
    let mut initiator_stream = connected.map_err(|error| {
        PairSessionError::Local(format!("connect loopback pairing listener: {error}"))
    })?;
    let (mut responder_stream, _) = accepted.map_err(|error| {
        PairSessionError::Local(format!("accept loopback pairing listener: {error}"))
    })?;
    let (initiator_result, responder_result) = tokio::join!(
        run_initiator_over_stream(
            &mut initiator_stream,
            &initiator,
            responder.identity.instance_id.clone(),
            open.pair_id,
            open.code.as_bytes(),
        ),
        run_responder_over_stream(&mut responder_stream, &mut window, &responder),
    );
    initiator_result?;
    responder_result
}

#[allow(
    dead_code,
    reason = "Task 14 drives responder windows from the TCP accept loop."
)]
impl PairingWindow {
    /// Open a responder window with fresh OS entropy for both its code and ID.
    pub(crate) fn open(pairing_window: Duration) -> Result<(Self, OpenPairing), PairSessionError> {
        Self::open_named(pairing_window, String::new())
    }

    fn open_named(
        pairing_window: Duration,
        display_name: String,
    ) -> Result<(Self, OpenPairing), PairSessionError> {
        let mut code_bytes = [0_u8; 5];
        getrandom::fill(&mut code_bytes)
            .map_err(|error| PairSessionError::Local(format!("generate pairing code: {error}")))?;
        let code = encode_crockford(code_bytes);

        let mut pair_id_bytes = [0_u8; 16];
        getrandom::fill(&mut pair_id_bytes)
            .map_err(|error| PairSessionError::Local(format!("generate pairing id: {error}")))?;
        let pair_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair_id_bytes);
        let expires_at = Instant::now() + pairing_window;
        let expires_at_wall = time::OffsetDateTime::now_utc()
            .checked_add(
                time::Duration::try_from(pairing_window)
                    .map_err(|error| PairSessionError::Local(error.to_string()))?,
            )
            .ok_or_else(|| PairSessionError::Local("pairing expiry is out of range".to_owned()))?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| PairSessionError::Local(error.to_string()))?;
        let open = OpenPairing {
            pair_id: pair_id.clone(),
            code: code.clone(),
            expires_at: expires_at_wall,
        };
        Ok((
            Self {
                pair_id,
                display_name,
                code: Zeroizing::new(code.into_bytes()),
                expires_at,
                attempts: 0,
                seen_initiator_nonces: HashSet::new(),
                cancelled: false,
                completed: false,
                in_flight: Arc::new(AtomicBool::new(false)),
                state: PairingState::Pairing,
                peer_instance_id: None,
            },
            open,
        ))
    }

    fn status(&self) -> PairingStatus {
        PairingStatus {
            pair_id: self.pair_id.clone(),
            state: self.state,
            peer_instance_id: self.peer_instance_id.clone(),
        }
    }

    fn begin(&self) -> Result<SessionFlight, PairSessionError> {
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| PairSessionError::Busy)?;
        Ok(SessionFlight {
            in_flight: Arc::clone(&self.in_flight),
        })
    }

    fn reject_before_spake(&mut self) -> Result<(), PairSessionError> {
        if self.cancelled {
            return Err(PairSessionError::Wire(PairError::Cancelled));
        }
        if Instant::now() >= self.expires_at {
            self.state = PairingState::Timeout;
            return Err(PairSessionError::Wire(PairError::CodeExpired));
        }
        if self.completed {
            return Err(PairSessionError::Wire(PairError::InvalidFrame));
        }
        if self.attempts >= MAX_PAIR_ATTEMPTS {
            self.state = PairingState::Error;
            return Err(PairSessionError::Wire(PairError::AttemptLimit));
        }
        self.attempts += 1;
        Ok(())
    }

    /// Cancel the responder window. No peer record is written by cancellation.
    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
        self.state = PairingState::Cancelled;
    }
}

#[allow(
    dead_code,
    reason = "Task 14 holds this guard across one TCP pairing session."
)]
struct SessionFlight {
    in_flight: Arc<AtomicBool>,
}

#[allow(
    dead_code,
    reason = "Task 14 holds this guard across one TCP pairing session."
)]
impl Drop for SessionFlight {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

#[allow(
    dead_code,
    reason = "Task 14 advances this state from inbound TCP frames."
)]
struct InitiatorState {
    spake: Option<Spake2<Ed25519Group>>,
    local: PeerIdentity,
    remote_instance_id: String,
    window_id: String,
    remote_display_name: Option<String>,
    remote_nonce: Option<[u8; NONCE_BYTES]>,
    remote_public_key: Option<[u8; 32]>,
    key: Option<Zeroizing<Vec<u8>>>,
}

#[allow(
    dead_code,
    reason = "Task 14 advances this state from inbound TCP frames."
)]
struct ResponderState {
    local: PeerIdentity,
    remote: PeerIdentity,
    key: Zeroizing<Vec<u8>>,
    confirmation_seen: bool,
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this when opening an outbound TCP pairing session."
)]
fn begin_initiator(
    local: &LocalPeer,
    remote_instance_id: String,
    window_id: String,
    code: &[u8],
) -> Result<(InitiatorState, Vec<Vec<u8>>), PairSessionError> {
    if local.identity.instance_id == remote_instance_id {
        return Err(PairSessionError::SelfPair);
    }
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce)
        .map_err(|error| PairSessionError::Local(format!("generate initiator nonce: {error}")))?;
    let (spake, outbound) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code),
        &Identity::new(PAIRING_CONTEXT),
    );
    let local_identity = PeerIdentity {
        instance_id: local.identity.instance_id.clone(),
        display_name: local.display_name.clone(),
        public_key: local.public_key(),
        nonce,
    };
    let frames = vec![
        PairFrame::PairHello {
            protocol_version: PAIR_PROTOCOL_VERSION,
            role: PairRole::Initiator,
            instance_id: local_identity.instance_id.clone(),
            display_name: local_identity.display_name.clone(),
            window_id: window_id.clone(),
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
        },
        PairFrame::Spake2Msg1 {
            message: base64::engine::general_purpose::STANDARD.encode(outbound),
        },
    ];
    Ok((
        InitiatorState {
            spake: Some(spake),
            local: local_identity,
            remote_instance_id,
            window_id,
            remote_display_name: None,
            remote_nonce: None,
            remote_public_key: None,
            key: None,
        },
        frames_to_wire(&frames),
    ))
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this on an inbound TCP PairHello."
)]
fn accept_hello(
    window: &mut PairingWindow,
    responder: &LocalPeer,
    frame: PairFrame,
) -> Result<PeerIdentity, PairSessionError> {
    window.reject_before_spake()?;
    let PairFrame::PairHello {
        protocol_version,
        role,
        instance_id,
        display_name,
        window_id,
        nonce,
    } = frame
    else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    if protocol_version != PAIR_PROTOCOL_VERSION {
        return Err(PairSessionError::Wire(PairError::ProtocolVersion));
    }
    if !matches!(role, PairRole::Initiator)
        || window_id != window.pair_id
        || display_name.is_empty()
        || instance_id == responder.identity.instance_id
    {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let instance_bytes = decode_base64url_key(&instance_id)?;
    let nonce = decode_exact(&nonce)?;
    if !window.seen_initiator_nonces.insert(nonce) {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    Ok(PeerIdentity {
        instance_id,
        display_name,
        public_key: instance_bytes,
        nonce,
    })
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this on an inbound TCP Spake2Msg1."
)]
fn responder_receive_msg1(
    window: &PairingWindow,
    responder: &LocalPeer,
    remote: PeerIdentity,
    frame: PairFrame,
) -> Result<(ResponderState, Vec<Vec<u8>>), PairSessionError> {
    let PairFrame::Spake2Msg1 { message } = frame else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let inbound = decode_base64(&message)?;
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce)
        .map_err(|error| PairSessionError::Local(format!("generate responder nonce: {error}")))?;
    let (spake, outbound) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(&window.code),
        &Identity::new(PAIRING_CONTEXT),
    );
    let key = spake
        .finish(&inbound)
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    let local = PeerIdentity {
        instance_id: responder.identity.instance_id.clone(),
        display_name: responder.display_name.clone(),
        public_key: responder.public_key(),
        nonce,
    };
    let frames = vec![
        PairFrame::PairHello {
            protocol_version: PAIR_PROTOCOL_VERSION,
            role: PairRole::Responder,
            instance_id: local.instance_id.clone(),
            display_name: local.display_name.clone(),
            window_id: window.pair_id.clone(),
            nonce: base64::engine::general_purpose::STANDARD.encode(local.nonce),
        },
        PairFrame::Spake2Msg2 {
            message: base64::engine::general_purpose::STANDARD.encode(outbound),
        },
        PairFrame::IdentityExchange {
            ed25519_pub: base64::engine::general_purpose::STANDARD.encode(local.public_key),
        },
    ];
    Ok((
        ResponderState {
            local,
            remote,
            key: Zeroizing::new(key),
            confirmation_seen: false,
        },
        frames_to_wire(&frames),
    ))
}

#[allow(dead_code, reason = "Task 14 invokes this on responder TCP frames.")]
fn initiator_receive_responder(
    state: &mut InitiatorState,
    frames: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, PairSessionError> {
    if frames.len() != 3 {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let hello = wire_to_frame(&frames[0])?;
    let PairFrame::PairHello {
        protocol_version,
        role,
        instance_id,
        display_name,
        window_id,
        nonce,
    } = hello
    else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    if protocol_version != PAIR_PROTOCOL_VERSION {
        return Err(PairSessionError::Wire(PairError::ProtocolVersion));
    }
    if !matches!(role, PairRole::Responder)
        || window_id != state.window_id
        || display_name.is_empty()
        || instance_id != state.remote_instance_id
    {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let expected_public_key = decode_base64url_key(&instance_id)?;
    let remote_nonce = decode_exact(&nonce)?;
    let msg2 = wire_to_frame(&frames[1])?;
    let PairFrame::Spake2Msg2 { message } = msg2 else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let inbound = decode_base64(&message)?;
    let key = state
        .spake
        .take()
        .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?
        .finish(&inbound)
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    let identity = wire_to_frame(&frames[2])?;
    let PairFrame::IdentityExchange { ed25519_pub } = identity else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let remote_public_key = decode_exact(&ed25519_pub)?;
    if remote_public_key != expected_public_key
        || instance_id_from_public_key(&remote_public_key) != state.remote_instance_id
        || VerifyingKey::from_bytes(&remote_public_key).is_err()
    {
        return Err(PairSessionError::Wire(PairError::InstanceIdConflict));
    }
    state.remote_public_key = Some(remote_public_key);
    state.remote_nonce = Some(remote_nonce);
    state.remote_display_name = Some(display_name);
    state.key = Some(Zeroizing::new(key));

    let transcript = transcript_for_initiator(state)?;
    let key = state
        .key
        .as_deref()
        .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?;
    let mac = confirmation(key, &transcript);
    Ok(frames_to_wire(&[
        PairFrame::IdentityExchange {
            ed25519_pub: base64::engine::general_purpose::STANDARD.encode(state.local.public_key),
        },
        PairFrame::KeyConfirm {
            mac: base64::engine::general_purpose::STANDARD.encode(mac),
        },
    ]))
}

#[allow(dead_code, reason = "Task 14 invokes this on initiator TCP frames.")]
fn responder_receive_initiator(
    state: &mut ResponderState,
    frames: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, PairSessionError> {
    if frames.len() != 2 {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let identity = wire_to_frame(&frames[0])?;
    let PairFrame::IdentityExchange { ed25519_pub } = identity else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let key = decode_exact(&ed25519_pub)?;
    if key != state.remote.public_key
        || instance_id_from_public_key(&key) != state.remote.instance_id
    {
        return Err(PairSessionError::Wire(PairError::InstanceIdConflict));
    }
    let confirm = wire_to_frame(&frames[1])?;
    verify_confirmation(&state.key, &transcript_for_responder(state)?, confirm)?;
    state.confirmation_seen = true;
    let mac = confirmation(&state.key, &transcript_for_responder(state)?);
    Ok(frames_to_wire(&[PairFrame::KeyConfirm {
        mac: base64::engine::general_purpose::STANDARD.encode(mac),
    }]))
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this on an inbound TCP KeyConfirm."
)]
fn initiator_receive_confirmation(
    state: &InitiatorState,
    frame: &[u8],
) -> Result<Vec<u8>, PairSessionError> {
    let transcript = transcript_for_initiator(state)?;
    verify_confirmation(
        state
            .key
            .as_deref()
            .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?,
        &transcript,
        wire_to_frame(frame)?,
    )?;
    Ok(frame_for_wire(&PairFrame::PairResult {
        accepted: true,
        error: None,
    }))
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this on an inbound TCP PairResult."
)]
fn responder_receive_result(
    frame: &[u8],
    state: &ResponderState,
) -> Result<Vec<u8>, PairSessionError> {
    if !state.confirmation_seen {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let PairFrame::PairResult {
        accepted: true,
        error: None,
    } = wire_to_frame(frame)?
    else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    Ok(frame_for_wire(&PairFrame::PairResult {
        accepted: true,
        error: None,
    }))
}

#[cfg(test)]
fn run_in_process_pairing(
    initiator: &LocalPeer,
    responder: &LocalPeer,
    window: &mut PairingWindow,
    code: &[u8],
    tamper_responder_identity: bool,
    tamper_responder_nonce: bool,
    replay_confirmation: bool,
) -> Result<(), PairSessionError> {
    let _flight = window.begin()?;
    let (mut initiator_state, initiator_start) = begin_initiator(
        initiator,
        responder.identity.instance_id.clone(),
        window.pair_id.clone(),
        code,
    )?;
    let remote = accept_hello(window, responder, wire_to_frame(&initiator_start[0])?)?;
    let (mut responder_state, responder_start) = responder_receive_msg1(
        window,
        responder,
        remote,
        wire_to_frame(&initiator_start[1])?,
    )?;
    let mut responder_start = responder_start;
    if tamper_responder_nonce {
        let PairFrame::PairHello { nonce, .. } = wire_to_frame(&responder_start[0])? else {
            unreachable!("responder emits PairHello");
        };
        let mut nonce: [u8; NONCE_BYTES] = decode_exact(&nonce)?;
        nonce[0] ^= 1;
        let PairFrame::PairHello {
            protocol_version,
            role,
            instance_id,
            display_name,
            window_id,
            ..
        } = wire_to_frame(&responder_start[0])?
        else {
            unreachable!("responder emits PairHello");
        };
        responder_start[0] = frame_for_wire(&PairFrame::PairHello {
            protocol_version,
            role,
            instance_id,
            display_name,
            window_id,
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
        });
    }
    if tamper_responder_identity {
        let PairFrame::IdentityExchange { ed25519_pub } = wire_to_frame(&responder_start[2])?
        else {
            unreachable!("responder emits identity exchange");
        };
        let mut public_key: [u8; 32] = decode_exact(&ed25519_pub)?;
        public_key[0] ^= 1;
        responder_start[2] = frame_for_wire(&PairFrame::IdentityExchange {
            ed25519_pub: base64::engine::general_purpose::STANDARD.encode(public_key),
        });
    }
    let initiator_confirm = initiator_receive_responder(&mut initiator_state, &responder_start)?;
    let responder_confirm = responder_receive_initiator(&mut responder_state, &initiator_confirm)?;
    if replay_confirmation {
        let mut replay = initiator_confirm.clone();
        replay[0].clone_from(&responder_confirm[0]);
        return responder_receive_initiator(&mut responder_state, &replay).map(|_| ());
    }
    let initiator_result = initiator_receive_confirmation(&initiator_state, &responder_confirm[0])?;
    let responder_result = responder_receive_result(&initiator_result, &responder_state)?;
    let PairFrame::PairResult {
        accepted: true,
        error: None,
    } = wire_to_frame(&responder_result)?
    else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };

    let responder_identity = PeerIdentity {
        instance_id: responder.identity.instance_id.clone(),
        display_name: responder.display_name.clone(),
        public_key: responder.public_key(),
        nonce: responder_state.local.nonce,
    };
    initiator.persist(&responder_identity)?;
    initiator_state.key.take();
    responder.persist(&initiator_state.local)?;
    window.completed = true;
    window.state = PairingState::Paired;
    window.peer_instance_id = Some(initiator.identity.instance_id.clone());
    tracing::info!(event = "pairing_confirmed");
    Ok(())
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this while confirming the initiator transcript."
)]
fn transcript_for_initiator(state: &InitiatorState) -> Result<Vec<u8>, PairSessionError> {
    let responder_nonce = state
        .remote_nonce
        .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?;
    let responder_public_key = state
        .remote_public_key
        .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?;
    build_pairing_transcript(
        PAIR_PROTOCOL_VERSION,
        &state.local.instance_id,
        &state.remote_instance_id,
        &state.local.display_name,
        state
            .remote_display_name
            .as_deref()
            .ok_or(PairSessionError::Wire(PairError::InvalidFrame))?,
        &state.local.public_key,
        &responder_public_key,
        &state.local.nonce,
        &responder_nonce,
    )
    .map_err(|error| PairSessionError::Local(error.to_string()))
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this while confirming the responder transcript."
)]
fn transcript_for_responder(state: &ResponderState) -> Result<Vec<u8>, PairSessionError> {
    build_pairing_transcript(
        PAIR_PROTOCOL_VERSION,
        &state.remote.instance_id,
        &state.local.instance_id,
        &state.remote.display_name,
        &state.local.display_name,
        &state.remote.public_key,
        &state.local.public_key,
        &state.remote.nonce,
        &state.local.nonce,
    )
    .map_err(|error| PairSessionError::Local(error.to_string()))
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this to emit TCP KeyConfirm frames."
)]
fn confirmation(key: &[u8], transcript: &[u8]) -> [u8; MAC_BYTES] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

#[allow(
    dead_code,
    reason = "Task 14 invokes this on inbound TCP KeyConfirm frames."
)]
fn verify_confirmation(
    key: &[u8],
    transcript: &[u8],
    frame: PairFrame,
) -> Result<(), PairSessionError> {
    let PairFrame::KeyConfirm { mac } = frame else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let received: [u8; MAC_BYTES] = decode_exact(&mac)?;
    let mut expected = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    expected.update(transcript);
    expected
        .verify_slice(&received)
        .map_err(|_| PairSessionError::Wire(PairError::KeyConfirmation))
}

fn encode_crockford(bytes: [u8; 5]) -> String {
    let value = u64::from_be_bytes([0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]);
    (0..8)
        .map(|shift| CROCKFORD_BASE32[((value >> (35 - shift * 5)) & 31) as usize] as char)
        .collect()
}

#[allow(
    dead_code,
    reason = "Task 14 validates exact-width fields from TCP frames."
)]
fn decode_exact<const N: usize>(encoded: &str) -> Result<[u8; N], PairSessionError> {
    let bytes = decode_base64(encoded)?;
    bytes
        .try_into()
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
}

#[allow(dead_code, reason = "Task 14 decodes byte fields from TCP frames.")]
fn decode_base64(encoded: &str) -> Result<Vec<u8>, PairSessionError> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
}

fn decode_base64url_key(encoded: &str) -> Result<[u8; 32], PairSessionError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?
        .try_into()
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
}

#[allow(dead_code, reason = "Task 14 writes length-prefixed TCP frames.")]
fn frame_for_wire(frame: &PairFrame) -> Vec<u8> {
    let payload = serde_json::to_vec(&frame).expect("PairFrame serialization is infallible");
    let length = u32::try_from(payload.len()).expect("PairFrame payload fits u32");
    let mut wire = Vec::with_capacity(payload.len() + 4);
    wire.extend_from_slice(&length.to_be_bytes());
    wire.extend_from_slice(&payload);
    wire
}

#[allow(dead_code, reason = "Task 14 writes ordered TCP handshake frames.")]
fn frames_to_wire(frames: &[PairFrame]) -> Vec<Vec<u8>> {
    frames.iter().map(frame_for_wire).collect()
}

#[allow(dead_code, reason = "Task 14 parses length-prefixed TCP frames.")]
fn wire_to_frame(wire: &[u8]) -> Result<PairFrame, PairSessionError> {
    if wire.len() < 5 {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let length_bytes = wire[..4]
        .try_into()
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    let length = u32::from_be_bytes(length_bytes);
    let payload = &wire[4..];
    if !(1..=MAX_PAIR_FRAME_BYTES).contains(&length)
        || usize::try_from(length).ok() != Some(payload.len())
    {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    serde_json::from_slice(payload).map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
}

/// Serialize and write exactly one length-prefixed pairing frame.
#[allow(
    dead_code,
    reason = "Task 14a-2's pairing lifecycle writes frames after a TCP connection opens."
)]
async fn write_pair_frame<W>(writer: &mut W, frame: &PairFrame) -> Result<(), PairSessionError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;

    let payload = serde_json::to_vec(frame)
        .map_err(|error| PairSessionError::Local(format!("serialize pairing frame: {error}")))?;
    let length = u32::try_from(payload.len())
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    if !(1..=MAX_PAIR_FRAME_BYTES).contains(&length) {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }

    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(|error| PairSessionError::Local(format!("write pairing frame length: {error}")))?;
    writer
        .write_all(&payload)
        .await
        .map_err(|error| PairSessionError::Local(format!("write pairing frame payload: {error}")))
}

/// Read and deserialize exactly one bounded length-prefixed pairing frame.
#[allow(
    dead_code,
    reason = "Task 14a-2's pairing lifecycle reads frames after a TCP connection opens."
)]
async fn read_pair_frame<R>(reader: &mut R) -> Result<PairFrame, PairSessionError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut length_bytes = [0_u8; 4];
    reader
        .read_exact(&mut length_bytes)
        .await
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    let length = u32::from_be_bytes(length_bytes);
    if !(1..=MAX_PAIR_FRAME_BYTES).contains(&length) {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }

    let payload_len =
        usize::try_from(length).map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    let mut payload = vec![0_u8; payload_len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    serde_json::from_slice(&payload).map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
}

#[cfg(test)]
struct PairingHarness {
    _root: tempfile::TempDir,
    initiator: LocalPeer,
    responder: LocalPeer,
    window: PairingWindow,
    tamper_identity: bool,
    tamper_nonce: bool,
}

#[cfg(test)]
struct LogCapture {
    result: Result<(), PairSessionError>,
    logs: String,
}

#[cfg(test)]
#[derive(Clone)]
struct CaptureLayer(Arc<Mutex<Vec<String>>>);

#[cfg(test)]
impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(visitor.0);
    }
}

#[cfg(test)]
#[derive(Default)]
struct EventVisitor(String);

#[cfg(test)]
impl tracing::field::Visit for EventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;

        let _ = write!(self.0, "{}={value:?};", field.name());
    }
}

#[cfg(test)]
impl PairingHarness {
    fn new() -> Result<Self, PairSessionError> {
        let root =
            tempfile::tempdir().map_err(|error| PairSessionError::Local(error.to_string()))?;
        let initiator = LocalPeer::load(root.path().join("initiator"), "Office Mac".to_owned())?;
        let responder = LocalPeer::load(root.path().join("responder"), "Living room".to_owned())?;
        let (window, _) = PairingWindow::open(Duration::from_secs(300))?;
        Ok(Self {
            _root: root,
            initiator,
            responder,
            window,
            tamper_identity: false,
            tamper_nonce: false,
        })
    }

    fn code(&self) -> &str {
        std::str::from_utf8(&self.window.code).expect("Crockford pairing code is UTF-8")
    }

    fn complete_with_code(&mut self, code: &str) -> Result<(), PairSessionError> {
        run_in_process_pairing(
            &self.initiator,
            &self.responder,
            &mut self.window,
            code.as_bytes(),
            self.tamper_identity,
            self.tamper_nonce,
            false,
        )
    }

    fn tamper_next_identity_exchange(&mut self) {
        self.tamper_identity = true;
    }

    fn tamper_next_responder_nonce(&mut self) {
        self.tamper_nonce = true;
    }

    fn replay_confirmation(&mut self, code: &str) -> Result<(), PairSessionError> {
        run_in_process_pairing(
            &self.initiator,
            &self.responder,
            &mut self.window,
            code.as_bytes(),
            self.tamper_identity,
            self.tamper_nonce,
            true,
        )
    }

    fn expire_window(&mut self) {
        self.window.expires_at = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("monotonic clock supports one-second test adjustment");
    }

    fn start_self_join(&self) -> Result<(), PairSessionError> {
        begin_initiator(
            &self.initiator,
            self.initiator.identity.instance_id.clone(),
            self.window.pair_id.clone(),
            self.code().as_bytes(),
        )
        .map(|_| ())
    }

    fn cancel(&mut self) {
        self.window.cancel();
    }

    fn secret_material_markers(&self) -> Vec<String> {
        vec![
            self.code().to_owned(),
            base64::engine::general_purpose::STANDARD.encode(self.initiator.public_key()),
            base64::engine::general_purpose::STANDARD.encode(self.responder.public_key()),
            lowercase_hex(&self.initiator.public_key()),
            lowercase_hex(&self.responder.public_key()),
        ]
    }

    fn complete_with_captured_logs(&mut self, code: &str) -> LogCapture {
        use tracing_subscriber::prelude::*;

        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&events)));
        let result =
            tracing::subscriber::with_default(subscriber, || self.complete_with_code(code));
        LogCapture {
            result,
            logs: events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .join("\n"),
        }
    }

    fn send_malformed_pairhello(&mut self) -> Result<(), PairSessionError> {
        let _flight = self.window.begin()?;
        let window_id = self.window.pair_id.clone();
        accept_hello(
            &mut self.window,
            &self.responder,
            PairFrame::PairHello {
                protocol_version: PAIR_PROTOCOL_VERSION,
                role: PairRole::Initiator,
                instance_id: "not-base64url".to_owned(),
                display_name: "Office Mac".to_owned(),
                window_id,
                nonce: base64::engine::general_purpose::STANDARD.encode([0_u8; NONCE_BYTES]),
            },
        )
        .map(|_| ())
    }

    fn reject_hello_after_connection_close(&mut self) -> Result<(), PairSessionError> {
        let _flight = self.window.begin()?;
        let (state, frames) = begin_initiator(
            &self.initiator,
            self.responder.identity.instance_id.clone(),
            self.window.pair_id.clone(),
            self.code().as_bytes(),
        )?;
        let _state = state;
        let remote = accept_hello(
            &mut self.window,
            &self.responder,
            wire_to_frame(&frames[0])?,
        )?;
        responder_receive_msg1(
            &self.window,
            &self.responder,
            remote,
            PairFrame::IdentityExchange {
                ed25519_pub: base64::engine::general_purpose::STANDARD
                    .encode(self.initiator.public_key()),
            },
        )
        .map(|_| ())
    }

    fn send_pairhello_version(&mut self, version: u16) -> Result<(), PairSessionError> {
        let _flight = self.window.begin()?;
        let window_id = self.window.pair_id.clone();
        accept_hello(
            &mut self.window,
            &self.responder,
            PairFrame::PairHello {
                protocol_version: version,
                role: PairRole::Initiator,
                instance_id: self.initiator.identity.instance_id.clone(),
                display_name: self.initiator.display_name.clone(),
                window_id,
                nonce: base64::engine::general_purpose::STANDARD.encode([1_u8; NONCE_BYTES]),
            },
        )
        .map(|_| ())
    }

    fn attempts(&self) -> usize {
        self.window.attempts
    }

    fn assert_mutual_persistence(&self) {
        let initiator =
            dormant_core::peers::load_peer_store(&self.initiator.state_dir.join("peers.json"))
                .expect("initiator peer store");
        let responder =
            dormant_core::peers::load_peer_store(&self.responder.state_dir.join("peers.json"))
                .expect("responder peer store");
        assert_eq!(initiator.peers.len(), 1);
        assert_eq!(responder.peers.len(), 1);
        assert_eq!(
            initiator.peers[0].instance_id,
            self.responder.identity.instance_id
        );
        assert_eq!(
            responder.peers[0].instance_id,
            self.initiator.identity.instance_id
        );
    }

    fn assert_nothing_persisted(&self) {
        assert!(!self.initiator.state_dir.join("peers.json").exists());
        assert!(!self.responder.state_dir.join("peers.json").exists());
    }
}

#[cfg(test)]
fn lowercase_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination_mdns::{AdvertisementHandle, BrowseEvent, BrowseStream};
    use dormant_core::coordination::CoordinationHandle;
    use tokio::io::AsyncWriteExt as _;

    #[derive(Clone, Default)]
    struct TransportFakeBackend {
        advertised: Arc<Mutex<Option<DiscoverAnnounce>>>,
    }

    struct TransportFakeAdvertisement(Arc<Mutex<Option<DiscoverAnnounce>>>);

    impl AdvertisementHandle for TransportFakeAdvertisement {}

    impl Drop for TransportFakeAdvertisement {
        fn drop(&mut self) {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    struct TransportFakeBrowse;

    impl BrowseStream for TransportFakeBrowse {
        fn try_next(&mut self) -> anyhow::Result<Option<BrowseEvent>> {
            Ok(None)
        }
    }

    impl MdnsBackend for TransportFakeBackend {
        fn advertise(
            &self,
            service: DiscoverAnnounce,
        ) -> anyhow::Result<Box<dyn AdvertisementHandle>> {
            *self
                .advertised
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(service);
            Ok(Box::new(TransportFakeAdvertisement(Arc::clone(
                &self.advertised,
            ))))
        }

        fn browse(&self) -> anyhow::Result<Box<dyn BrowseStream>> {
            Ok(Box::new(TransportFakeBrowse))
        }
    }

    type TransportHarness = (
        tempfile::TempDir,
        Arc<PairingManager>,
        PairingTransport<TransportFakeBackend>,
        Arc<Mutex<Option<DiscoverAnnounce>>>,
    );

    fn transport_harness(enabled: bool, window: Duration) -> TransportHarness {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(PairingManager::new(root.path(), enabled, window).unwrap());
        let backend = TransportFakeBackend::default();
        let advertised = Arc::clone(&backend.advertised);
        let discovery = PairDiscovery::new(
            backend,
            manager.identity.as_ref().map_or_else(
                || "disabled".to_owned(),
                |identity| identity.instance_id.clone(),
            ),
            CoordinationHandle::new([]),
        );
        let transport = PairingTransport::new_with_bind_ip_for_test(
            Arc::clone(&manager),
            discovery,
            0,
            "127.0.0.1".parse().unwrap(),
            tokio_util::sync::CancellationToken::new(),
        );
        (root, manager, transport, advertised)
    }

    fn advertised_addr(advertised: &Arc<Mutex<Option<DiscoverAnnounce>>>) -> SocketAddr {
        let port = advertised
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .unwrap()
            .pairing_port;
        SocketAddr::new("127.0.0.1".parse().unwrap(), port)
    }

    async fn wait_for_advertisement_withdrawal(advertised: &Arc<Mutex<Option<DiscoverAnnounce>>>) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if advertised
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_none()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("advertisement should withdraw before the test deadline");
    }

    macro_rules! assert_wire_error {
        ($actual:expr, $expected:pat) => {
            assert!(matches!($actual, Err(PairSessionError::Wire($expected))));
        };
    }

    #[tokio::test]
    async fn frame_codec_roundtrips() {
        let frame = PairFrame::PairResult {
            accepted: true,
            error: None,
        };
        let (mut writer, mut reader) = tokio::io::duplex(1024);

        write_pair_frame(&mut writer, &frame).await.unwrap();
        let decoded = read_pair_frame(&mut reader).await.unwrap();

        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(frame).unwrap()
        );
    }

    #[tokio::test]
    async fn frame_codec_rejects_oversized_without_alloc() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(&(MAX_PAIR_FRAME_BYTES + 1).to_be_bytes())
            .await
            .unwrap();

        let result =
            tokio::time::timeout(Duration::from_millis(50), read_pair_frame(&mut reader)).await;

        assert!(matches!(
            result,
            Ok(Err(PairSessionError::Wire(PairError::InvalidFrame)))
        ));
    }

    #[tokio::test]
    async fn frame_codec_rejects_truncated_stream() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_all(&10_u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"bad").await.unwrap();
        drop(writer);

        assert_wire_error!(read_pair_frame(&mut reader).await, PairError::InvalidFrame);
    }

    #[tokio::test]
    async fn frame_codec_rejects_bad_json() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_all(&3_u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"{{}").await.unwrap();
        drop(writer);

        assert_wire_error!(read_pair_frame(&mut reader).await, PairError::InvalidFrame);
    }

    #[tokio::test]
    async fn two_daemons_pair_over_real_stream() {
        let mut harness = PairingHarness::new().unwrap();
        let code = harness.code().as_bytes().to_vec();
        let responder_id = harness.responder.identity.instance_id.clone();
        let window_id = harness.window.pair_id.clone();
        let (mut initiator_stream, mut responder_stream) = tokio::io::duplex(131_072);

        let (initiator, responder) = tokio::join!(
            run_initiator_over_stream(
                &mut initiator_stream,
                &harness.initiator,
                responder_id,
                window_id,
                &code,
            ),
            run_responder_over_stream(
                &mut responder_stream,
                &mut harness.window,
                &harness.responder,
            ),
        );

        assert!(initiator.is_ok(), "initiator: {initiator:?}");
        assert!(responder.is_ok(), "responder: {responder:?}");
        assert_eq!(harness.window.state, PairingState::Paired);
        assert!(harness.initiator.state_dir.join("peers.json").exists());
        assert!(harness.responder.state_dir.join("peers.json").exists());
    }

    #[tokio::test]
    async fn wrong_code_over_stream_persists_nothing() {
        let mut harness = PairingHarness::new().unwrap();
        let responder_id = harness.responder.identity.instance_id.clone();
        let window_id = harness.window.pair_id.clone();
        let (mut initiator_stream, mut responder_stream) = tokio::io::duplex(131_072);

        let result = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                run_initiator_over_stream(
                    &mut initiator_stream,
                    &harness.initiator,
                    responder_id,
                    window_id,
                    b"WRONG-CODE",
                ),
                run_responder_over_stream(
                    &mut responder_stream,
                    &mut harness.window,
                    &harness.responder,
                ),
            )
        })
        .await;

        assert!(
            result.is_err()
                || result
                    .is_ok_and(|(initiator, responder)| initiator.is_err() || responder.is_err())
        );
        assert!(!harness.initiator.state_dir.join("peers.json").exists());
        assert!(!harness.responder.state_dir.join("peers.json").exists());
    }

    #[tokio::test]
    async fn two_daemons_pair_over_real_tcp() {
        let (root, manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        assert_eq!(address.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());

        let initiator =
            LocalPeer::load(root.path().join("initiator"), "Initiator".to_owned()).unwrap();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        run_initiator_over_stream(
            &mut stream,
            &initiator,
            manager.identity.as_ref().unwrap().instance_id.clone(),
            open.pair_id.clone(),
            open.code.as_bytes(),
        )
        .await
        .unwrap();
        wait_for_advertisement_withdrawal(&advertised).await;

        assert!(initiator.state_dir.join("peers.json").exists());
        assert!(root.path().join("peers.json").exists());
        assert_eq!(
            manager.status(&open.pair_id).unwrap().state,
            PairingState::Paired
        );
    }

    #[tokio::test]
    async fn out_of_order_frame_over_tcp_closes_not_panics() {
        let (_root, _manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let _open = transport.open("Responder".to_owned()).await.unwrap();
        let mut stream = tokio::net::TcpStream::connect(advertised_addr(&advertised))
            .await
            .unwrap();
        write_pair_frame(
            &mut stream,
            &PairFrame::PairResult {
                accepted: true,
                error: None,
            },
        )
        .await
        .unwrap();
        let closed =
            tokio::time::timeout(Duration::from_secs(1), read_pair_frame(&mut stream)).await;
        assert!(matches!(
            closed,
            Ok(Err(PairSessionError::Wire(PairError::InvalidFrame)))
        ));
    }

    #[tokio::test]
    async fn attempt_limit_survives_reconnects() {
        let (root, manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let open = transport.open("Responder".to_owned()).await.unwrap();
        let attacker =
            LocalPeer::load(root.path().join("attacker"), "Attacker".to_owned()).unwrap();
        let address = advertised_addr(&advertised);
        for attempt in 0_u8..11 {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let mut nonce = [0_u8; NONCE_BYTES];
            nonce[0] = attempt;
            write_pair_frame(
                &mut stream,
                &PairFrame::PairHello {
                    protocol_version: PAIR_PROTOCOL_VERSION,
                    role: PairRole::Initiator,
                    instance_id: attacker.identity.instance_id.clone(),
                    display_name: attacker.display_name.clone(),
                    window_id: open.pair_id.clone(),
                    nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
                },
            )
            .await
            .unwrap();
            stream.shutdown().await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if manager.status(&open.pair_id).unwrap().state == PairingState::Error {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the eleventh hello must trip the window attempt limit");
        assert!(!root.path().join("peers.json").exists());
    }

    #[tokio::test]
    async fn window_expiry_closes_listener_and_withdraws_advertisement() {
        let (_root, _manager, transport, advertised) =
            transport_harness(true, Duration::from_millis(80));
        let _open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        wait_for_advertisement_withdrawal(&advertised).await;
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn cancel_closes_listener_persists_nothing() {
        let (root, _manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        assert_eq!(
            transport.cancel(&open.pair_id).unwrap().state,
            PairingState::Cancelled
        );
        wait_for_advertisement_withdrawal(&advertised).await;
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        assert!(!root.path().join("peers.json").exists());
    }

    #[tokio::test]
    async fn disabled_coordination_opens_no_listener() {
        let (_root, _manager, transport, advertised) =
            transport_harness(false, Duration::from_secs(2));
        assert!(matches!(
            transport.open("Responder".to_owned()).await,
            Err(PairSessionError::Disabled)
        ));
        assert!(
            advertised
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );
    }

    #[tokio::test]
    async fn slow_connection_times_out() {
        let (root, manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(3));
        let open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        let slow = tokio::net::TcpStream::connect(address).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        drop(slow);

        let initiator =
            LocalPeer::load(root.path().join("initiator"), "Initiator".to_owned()).unwrap();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        run_initiator_over_stream(
            &mut stream,
            &initiator,
            manager.identity.as_ref().unwrap().instance_id.clone(),
            open.pair_id,
            open.code.as_bytes(),
        )
        .await
        .unwrap();
        assert!(initiator.state_dir.join("peers.json").exists());
    }

    #[tokio::test]
    async fn parallel_slow_connections_do_not_wedge_window() {
        let (root, manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        let mut slow = Vec::new();
        for _ in 0..6 {
            slow.push(tokio::net::TcpStream::connect(address).await.unwrap());
        }
        let initiator =
            LocalPeer::load(root.path().join("initiator"), "Initiator".to_owned()).unwrap();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        tokio::time::timeout(
            Duration::from_millis(700),
            run_initiator_over_stream(
                &mut stream,
                &initiator,
                manager.identity.as_ref().unwrap().instance_id.clone(),
                open.pair_id,
                open.code.as_bytes(),
            ),
        )
        .await
        .expect("legitimate peer should not wait for serial slow-connection timeouts")
        .unwrap();
        drop(slow);
        assert!(initiator.state_dir.join("peers.json").exists());
    }

    #[tokio::test]
    async fn connection_cap_per_window_enforced() {
        let (_root, _manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let _open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        let mut connections = Vec::new();
        for _ in 0..MAX_PAIRING_CONNS_PER_WINDOW {
            connections.push(tokio::net::TcpStream::connect(address).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        drop(connections);
    }

    #[tokio::test]
    async fn concurrent_correct_code_connections_pair_at_most_once() {
        let (root, manager, transport, advertised) =
            transport_harness(true, Duration::from_secs(2));
        let open = transport.open("Responder".to_owned()).await.unwrap();
        let address = advertised_addr(&advertised);
        let initiator_a = LocalPeer::load(root.path().join("initiator-a"), "A".to_owned()).unwrap();
        let initiator_b = LocalPeer::load(root.path().join("initiator-b"), "B".to_owned()).unwrap();
        let (mut stream_a, mut stream_b) = tokio::join!(
            tokio::net::TcpStream::connect(address),
            tokio::net::TcpStream::connect(address),
        );
        let (first, second) = tokio::join!(
            run_initiator_over_stream(
                stream_a.as_mut().unwrap(),
                &initiator_a,
                manager.identity.as_ref().unwrap().instance_id.clone(),
                open.pair_id.clone(),
                open.code.as_bytes(),
            ),
            run_initiator_over_stream(
                stream_b.as_mut().unwrap(),
                &initiator_b,
                manager.identity.as_ref().unwrap().instance_id.clone(),
                open.pair_id,
                open.code.as_bytes(),
            ),
        );
        assert!(first.is_ok() ^ second.is_ok());
        let peers: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("peers.json")).unwrap())
                .unwrap();
        assert_eq!(peers["peers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn correct_code_pairs_mutually_and_persists_both_keys() {
        let mut pairing = PairingHarness::new().unwrap();

        let code = pairing.code().to_owned();
        pairing.complete_with_code(&code).unwrap();

        pairing.assert_mutual_persistence();
    }

    #[test]
    fn wrong_code_persists_nothing() {
        let mut pairing = PairingHarness::new().unwrap();

        assert_wire_error!(
            pairing.complete_with_code("00000000"),
            PairError::KeyConfirmation
        );

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn tampered_public_key_fails_identity_binding() {
        let mut pairing = PairingHarness::new().unwrap();
        pairing.tamper_next_identity_exchange();

        let code = pairing.code().to_owned();
        // Pubkey↔claimed-id fails before MAC confirmation; nonce tampering below isolates MAC.
        assert_wire_error!(
            pairing.complete_with_code(&code),
            PairError::InstanceIdConflict
        );

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn tampered_nonce_fails_transcript_confirmation() {
        let mut pairing = PairingHarness::new().unwrap();
        pairing.tamper_next_responder_nonce();

        let code = pairing.code().to_owned();
        assert_wire_error!(
            pairing.complete_with_code(&code),
            PairError::KeyConfirmation
        );

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn replayed_confirmation_is_rejected() {
        let mut pairing = PairingHarness::new().unwrap();

        let code = pairing.code().to_owned();
        assert_wire_error!(pairing.replay_confirmation(&code), PairError::InvalidFrame);

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn expired_code_is_rejected() {
        let mut pairing = PairingHarness::new().unwrap();
        pairing.expire_window();

        let code = pairing.code().to_owned();
        assert_wire_error!(pairing.complete_with_code(&code), PairError::CodeExpired);

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn self_pair_is_rejected() {
        let pairing = PairingHarness::new().unwrap();

        assert!(matches!(
            pairing.start_self_join(),
            Err(PairSessionError::SelfPair)
        ));

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn cancel_persists_nothing() {
        let mut pairing = PairingHarness::new().unwrap();
        pairing.cancel();

        let code = pairing.code().to_owned();
        assert_wire_error!(pairing.complete_with_code(&code), PairError::Cancelled);

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn pairing_logs_contain_no_code_or_key_material() {
        let mut pairing = PairingHarness::new().unwrap();
        let secret_material = pairing.secret_material_markers();

        let code = pairing.code().to_owned();
        let capture = pairing.complete_with_captured_logs(&code);

        assert!(capture.result.is_ok());
        for secret in secret_material {
            assert!(!capture.logs.contains(&secret));
        }
    }

    #[test]
    fn malformed_pairhello_is_rejected_not_panicked() {
        let mut pairing = PairingHarness::new().unwrap();

        assert_wire_error!(pairing.send_malformed_pairhello(), PairError::InvalidFrame);

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn attempt_limit_enforced_per_window() {
        let mut pairing = PairingHarness::new().unwrap();

        for _ in 0..10 {
            assert_wire_error!(
                pairing.reject_hello_after_connection_close(),
                PairError::InvalidFrame
            );
        }
        assert_wire_error!(
            pairing.reject_hello_after_connection_close(),
            PairError::AttemptLimit
        );
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut pairing = PairingHarness::new().unwrap();

        assert_wire_error!(
            pairing.send_pairhello_version(1),
            PairError::ProtocolVersion
        );

        pairing.assert_nothing_persisted();
    }

    #[test]
    fn wrong_code_gets_one_guess() {
        let mut pairing = PairingHarness::new().unwrap();

        assert_wire_error!(
            pairing.complete_with_code("00000000"),
            PairError::KeyConfirmation
        );
        assert_eq!(pairing.attempts(), 1);
        pairing.assert_nothing_persisted();
    }

    #[test]
    fn out_of_order_confirmation_is_invalid_frame_not_panic() {
        let pairing = PairingHarness::new().unwrap();
        let (state, _) = begin_initiator(
            &pairing.initiator,
            pairing.responder.identity.instance_id.clone(),
            pairing.window.pair_id.clone(),
            pairing.code().as_bytes(),
        )
        .unwrap();
        let premature = frame_for_wire(&PairFrame::KeyConfirm {
            mac: base64::engine::general_purpose::STANDARD.encode([0_u8; MAC_BYTES]),
        });

        assert_wire_error!(
            initiator_receive_confirmation(&state, &premature),
            PairError::InvalidFrame
        );
    }
}
