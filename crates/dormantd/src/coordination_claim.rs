//! Authenticated claim-protocol transport over bounded TCP connections.
//!
//! A connection carries exactly one signed frame and may carry at most one response. The
//! supervisor authenticates inbound frames before handing them to application code; callers send
//! best-effort responses through the single-peer methods.

use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dormant_core::{
    claim::{ClaimFrame, ClaimMessage, Epoch, ReplayWindow},
    peers::{InstanceIdentity, PeerRecord, PeerStoreError, load_peer_store, upsert_peer},
};
use ed25519_dalek::VerifyingKey;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};

use crate::coordination_frame::{read_frame, write_frame};

const MAX_PREAUTH_CONNECTIONS: usize = 4;
const MAX_CONNECTIONS_PER_IP_MINUTE: usize = 10;
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Paired identity and last known claim endpoint used by the transport.
#[derive(Debug, Clone)]
pub struct ClaimPeer {
    /// Stable instance ID derived from the peer's verifying key.
    pub instance_id: String,
    /// Ed25519 key ratified by pairing.
    pub verifying_key: VerifyingKey,
    /// Last observed peer address; only its IP is used for claim dialing.
    pub last_addr: Option<SocketAddr>,
    /// Advisory endpoint learned from unsigned mDNS; never persisted.
    pub dns_addr: Option<SocketAddr>,
    /// Claim listener port verified from an authenticated connection.
    pub claim_port: Option<u16>,
    /// Claim listener port learned from unsigned mDNS; advisory — never persisted.
    pub dns_port: Option<u16>,
    /// Peer boot epoch learned from unsigned mDNS; advisory — never persisted.
    /// `None` when the remote predates the per-peer addressing fix or when
    /// mDNS has not yet resolved the peer.
    ///
    /// Same advisory-only security boundary as `dns_addr` / `dns_port`:
    /// unauthenticated mDNS data must not overwrite durable verified state.
    /// A wrong epoch from a hostile advertiser yields a rejected frame
    /// (`DoS` at worst), never an accepted one.
    pub dns_epoch: Option<Epoch>,
}

/// Outcome of one concurrent claim fanout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FanoutResult {
    /// Peers whose TCP connection and frame write both succeeded.
    pub contacted: usize,
    /// Peers skipped because no usable endpoint was known.
    pub skipped_no_endpoint: usize,
    /// Addressable peers skipped because no recipient epoch was known.
    pub skipped_no_epoch: usize,
    /// Per-peer frames that could not be signed.
    pub sign_failed: usize,
    /// Signed frames whose dial or frame write failed.
    pub dial_failed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    Verified,
    Dns,
}

impl EndpointKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Dns => "dns",
        }
    }
}

/// Persistent paired-peer store with a live snapshot for claim transport consumers.
pub(crate) struct PeerStoreFeed {
    path: PathBuf,
    store: Mutex<dormant_core::peers::PeerStore>,
    sender: watch::Sender<Vec<ClaimPeer>>,
}

impl PeerStoreFeed {
    /// Load the persisted peer store and create a feed seeded from its records.
    pub(crate) fn load(state_dir: &Path) -> Result<Self, PeerStoreError> {
        let path = state_dir.join("peers.json");
        let store = load_peer_store(&path)?;
        let peers = claim_peers(store.peers.clone())?;
        let (sender, _) = watch::channel(peers);
        Ok(Self {
            path,
            store: Mutex::new(store),
            sender,
        })
    }

    /// Subscribe to all persisted-peer changes.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Vec<ClaimPeer>> {
        self.sender.subscribe()
    }

    /// Persist a pairing result and publish the resulting peer snapshot.
    pub(crate) fn upsert(&self, record: PeerRecord) -> Result<(), PeerStoreError> {
        upsert_peer(&self.path, record.clone())?;
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = store
            .peers
            .iter_mut()
            .find(|existing| existing.instance_id == record.instance_id)
        {
            *existing = record;
        } else {
            store.peers.push(record);
        }
        self.publish(&store)
    }

    /// Persist a signature-verified address only when it has changed.
    pub(crate) fn refresh_verified_address(
        &self,
        instance_id: &str,
        address: SocketAddr,
    ) -> Result<(), PeerStoreError> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = store
            .peers
            .iter_mut()
            .find(|peer| peer.instance_id == instance_id)
        else {
            return Err(PeerStoreError::Invalid {
                detail: "cannot refresh endpoint for an unknown peer".to_owned(),
            });
        };
        if record.last_addr == Some(address) {
            return Ok(());
        }
        record.last_addr = Some(address);
        upsert_peer(&self.path, record.clone())?;
        self.publish(&store)
    }

    /// Publish an advisory mDNS address, port, and boot epoch without
    /// mutating the durable peer record.
    pub(crate) fn refresh_dns_address(
        &self,
        instance_id: &str,
        address: SocketAddr,
        epoch: Option<Epoch>,
    ) {
        self.sender.send_modify(|peers| {
            if let Some(peer) = peers
                .iter_mut()
                .find(|peer| peer.instance_id == instance_id)
            {
                peer.dns_addr = Some(address);
                peer.dns_port = Some(address.port());
                peer.dns_epoch = epoch;
            }
        });
    }

    fn publish(&self, store: &dormant_core::peers::PeerStore) -> Result<(), PeerStoreError> {
        let dns_info: HashMap<_, _> = self
            .sender
            .borrow()
            .iter()
            .filter_map(|peer| {
                peer.dns_addr.map(|address| {
                    (
                        peer.instance_id.clone(),
                        (address, peer.dns_port, peer.dns_epoch.clone()),
                    )
                })
            })
            .collect();
        let mut peers = claim_peers(store.peers.clone())?;
        for peer in &mut peers {
            if let Some((address, port, epoch)) = dns_info.get(&peer.instance_id).cloned() {
                peer.dns_addr = Some(address);
                peer.dns_port = port;
                peer.dns_epoch = epoch;
            }
        }
        self.sender.send_replace(peers);
        Ok(())
    }
}

fn claim_peers(records: Vec<PeerRecord>) -> Result<Vec<ClaimPeer>, PeerStoreError> {
    records
        .into_iter()
        .map(|record| {
            let key =
                STANDARD
                    .decode(&record.ed25519_pub)
                    .map_err(|_| PeerStoreError::Invalid {
                        detail: "paired peer public key is not valid base64".to_owned(),
                    })?;
            let key: [u8; 32] = key.try_into().map_err(|_| PeerStoreError::Invalid {
                detail: "paired peer public key must be 32 bytes".to_owned(),
            })?;
            let verifying_key =
                VerifyingKey::from_bytes(&key).map_err(|_| PeerStoreError::Invalid {
                    detail: "paired peer public key is invalid".to_owned(),
                })?;
            Ok(ClaimPeer {
                instance_id: record.instance_id,
                verifying_key,
                last_addr: record.last_addr,
                dns_addr: None,
                claim_port: record.claim_port,
                dns_port: None,
                dns_epoch: None,
            })
        })
        .collect()
}

/// Fully injected dependencies for a claim transport supervisor.
pub struct ClaimTransportDeps {
    /// Local paired-instance identity.
    pub identity: Arc<InstanceIdentity>,
    /// Validated epoch for this daemon run.
    pub boot_epoch: Epoch,
    /// Live paired-peer snapshot.
    pub peers: watch::Receiver<Vec<ClaimPeer>>,
    /// Address used for listener binds.
    pub bind_address: IpAddr,
    /// Requested listener port; `None` or zero requests an ephemeral port.
    pub fixed_port: Option<u16>,
    /// Whether coordination accepts claim traffic at startup.
    pub enabled: bool,
    /// Persists an authenticated peer's freshly observed remote address.
    pub on_peer_addr: Box<dyn Fn(String, SocketAddr) + Send + Sync>,
}

/// Control surface for the claim listener supervisor.
pub struct ClaimTransportHandle {
    commands: mpsc::Sender<Command>,
    inbound: Mutex<Option<mpsc::Receiver<ClaimFrame>>>,
    port: Arc<AtomicU16>,
    identity: Arc<InstanceIdentity>,
    boot_epoch: Epoch,
    sender_epoch: String,
    listener_port: watch::Receiver<Option<u16>>,
    peers: Arc<RwLock<Vec<ClaimPeer>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

enum Command {
    Ensure(oneshot::Sender<io::Result<u16>>),
    Release,
    Update {
        enabled: bool,
        address: IpAddr,
        port: Option<u16>,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Shutdown(oneshot::Sender<()>),
}

struct Supervisor {
    identity: Arc<InstanceIdentity>,
    boot_epoch: Epoch,
    peer_watch: watch::Receiver<Vec<ClaimPeer>>,
    peers: Arc<RwLock<Vec<ClaimPeer>>>,
    bind_address: IpAddr,
    fixed_port: Option<u16>,
    enabled: bool,
    provisional_hold: bool,
    listener: Option<TcpListener>,
    port: Arc<AtomicU16>,
    listener_port: watch::Sender<Option<u16>>,
    commands: mpsc::Receiver<Command>,
    inbound: mpsc::Sender<ClaimFrame>,
    on_peer_addr: Arc<dyn Fn(String, SocketAddr) + Send + Sync>,
    replay: Arc<Mutex<HashMap<(String, String), ReplayWindow>>>,
}

/// Spawn a parked claim transport supervisor.
#[must_use]
pub fn spawn(deps: ClaimTransportDeps) -> ClaimTransportHandle {
    let initial_peers = deps.peers.borrow().clone();
    let peers = Arc::new(RwLock::new(initial_peers));
    let port = Arc::new(AtomicU16::new(0));
    let (listener_port, listener_port_rx) = watch::channel(None);
    let (command_tx, command_rx) = mpsc::channel(16);
    let (inbound_tx, inbound_rx) = mpsc::channel(32);
    let sender_epoch = deps.boot_epoch.as_str().to_owned();
    let identity = Arc::clone(&deps.identity);
    let supervisor = Supervisor {
        identity: deps.identity,
        boot_epoch: deps.boot_epoch.clone(),
        peer_watch: deps.peers,
        peers: Arc::clone(&peers),
        bind_address: deps.bind_address,
        fixed_port: normalize_port(deps.fixed_port),
        enabled: deps.enabled,
        provisional_hold: false,
        listener: None,
        port: Arc::clone(&port),
        listener_port,
        commands: command_rx,
        inbound: inbound_tx,
        on_peer_addr: Arc::from(deps.on_peer_addr),
        replay: Arc::new(Mutex::new(HashMap::new())),
    };
    let task = tokio::spawn(supervisor.run());
    ClaimTransportHandle {
        commands: command_tx,
        inbound: Mutex::new(Some(inbound_rx)),
        port,
        identity,
        boot_epoch: deps.boot_epoch.clone(),
        sender_epoch,
        listener_port: listener_port_rx,
        peers,
        task: Mutex::new(Some(task)),
    }
}

impl ClaimTransportHandle {
    /// Bind the listener if parked and retain it independently of peer count.
    ///
    /// # Errors
    ///
    /// Returns the socket bind error or [`io::ErrorKind::BrokenPipe`] if the supervisor stopped.
    pub async fn ensure_provisional_listener(&self) -> io::Result<u16> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(Command::Ensure(reply_tx))
            .await
            .map_err(|_| supervisor_stopped())?;
        reply_rx.await.map_err(|_| supervisor_stopped())?
    }

    /// Release the provisional hold, parking only when the peer snapshot is empty.
    pub async fn release_provisional(&self) {
        let _ = self.commands.send(Command::Release).await;
    }

    /// Return the active listener port, or `None` while parked.
    #[must_use]
    pub fn provisional_port(&self) -> Option<u16> {
        match self.port.load(Ordering::Acquire) {
            0 => None,
            port => Some(port),
        }
    }

    /// Return the current daemon boot epoch for authenticated claim peers.
    #[must_use]
    pub fn boot_epoch(&self) -> &Epoch {
        &self.boot_epoch
    }

    /// Subscribe to bound-listener transitions for lifecycle-coupled services.
    #[must_use]
    pub fn subscribe_listener_port(&self) -> watch::Receiver<Option<u16>> {
        self.listener_port.clone()
    }

    /// Apply coordination enablement and rebind using swap-before-close ordering.
    ///
    /// # Errors
    ///
    /// Returns a bind error without disturbing the active listener.
    pub async fn update_config(
        &self,
        enabled: bool,
        address: IpAddr,
        port: Option<u16>,
    ) -> io::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(Command::Update {
                enabled,
                address,
                port: normalize_port(port),
                reply: reply_tx,
            })
            .await
            .map_err(|_| supervisor_stopped())?;
        reply_rx.await.map_err(|_| supervisor_stopped())?
    }

    /// Take the sole receiver for authenticated inbound frames.
    /// A second call returns an already-closed placeholder receiver.
    #[must_use]
    pub fn inbound(&self) -> mpsc::Receiver<ClaimFrame> {
        self.inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| mpsc::channel(1).1)
    }

    /// Concurrently send one signed request to every peer with a usable
    /// endpoint and a known boot epoch.
    ///
    /// Each peer receives its own frame — signed with the peer's instance id
    /// and current boot epoch (from the mDNS presence record). Peers whose
    /// epoch is unknown are skipped with a `claim_peer_no_epoch` log anchor.
    /// This replaces the pre-Bug-C/D broadcast pattern: every frame is now
    /// addressed to a specific recipient, preserving the anti-relay binding
    /// on the signed envelope.
    pub async fn fanout_request(
        &self,
        counter: u64,
        nonce: &str,
        message: &ClaimMessage,
    ) -> FanoutResult {
        let peers = self.peer_snapshot();
        let mut dials = JoinSet::new();
        let mut result = FanoutResult::default();
        for peer in peers {
            if peer_endpoints(&peer).is_empty() {
                result.skipped_no_endpoint += 1;
                tracing::info!(event = "claim_peer_no_port", peer = %peer.instance_id);
                continue;
            }
            let Some(ref recipient_epoch) = peer.dns_epoch else {
                result.skipped_no_epoch += 1;
                tracing::info!(event = "claim_peer_no_epoch", peer = %peer.instance_id);
                continue;
            };
            let frame = match ClaimFrame::sign(
                &self.identity,
                self.sender_epoch.clone(),
                peer.instance_id.clone(),
                recipient_epoch.as_str().to_owned(),
                counter,
                nonce.to_owned(),
                message.clone(),
            ) {
                Ok(frame) => frame,
                Err(error) => {
                    result.sign_failed += 1;
                    tracing::warn!(event = "claim_frame_sign_failed", %error);
                    continue;
                }
            };
            dials.spawn(async move {
                let peer_id = peer.instance_id.clone();
                (peer_id, send_to_peer(&peer, &frame).await)
            });
        }
        while let Some(task) = dials.join_next().await {
            match task {
                Ok((_peer, Ok(()))) => result.contacted += 1,
                Ok((peer, Err(error))) => {
                    result.dial_failed += 1;
                    tracing::warn!(event = "claim_peer_send_failed", %peer, %error);
                }
                Err(error) => {
                    result.dial_failed += 1;
                    tracing::warn!(event = "claim_peer_send_task_failed", %error);
                }
            }
        }
        result
    }

    /// Best-effort delivery of a signed claim-abort frame to one peer.
    pub async fn send_abort(&self, peer_instance_id: &str, frame: &ClaimFrame) {
        self.send_to_peer(peer_instance_id, frame).await;
    }

    /// Best-effort delivery of a signed release-failed frame to one peer.
    pub async fn send_release_failed(&self, peer_instance_id: &str, frame: &ClaimFrame) {
        self.send_to_peer(peer_instance_id, frame).await;
    }

    /// Best-effort delivery of a signed acquire-ready frame to one peer.
    pub async fn send_acquire_ready(&self, peer_instance_id: &str, frame: &ClaimFrame) {
        self.send_to_peer(peer_instance_id, frame).await;
    }

    /// Best-effort delivery of a signed response frame to one peer.
    /// Used by the owner-side `IdleQuery` handler to reply with an
    /// `IdleReport` and by any future point-to-point response path.
    pub async fn send_response(&self, peer_instance_id: &str, frame: &ClaimFrame) {
        self.send_to_peer(peer_instance_id, frame).await;
    }

    /// Snapshot the currently-known paired peers (read-only). The
    /// runtime driver uses this to resolve the per-display owner /
    /// requester peer instance id without the supervisor holding
    /// the snapshot in a side channel.
    #[must_use]
    pub fn snapshot_peers(&self) -> Vec<ClaimPeer> {
        self.peer_snapshot()
    }

    /// Count peers eligible for an addressed fanout before any dial is attempted.
    /// A peer needs both a usable endpoint and the advisory mDNS epoch used only
    /// to address the signed request; [`FanoutResult::contacted`] remains the
    /// authoritative post-dial count.
    #[must_use]
    pub fn addressable_peer_count(&self) -> usize {
        self.peer_snapshot()
            .iter()
            .filter(|peer| peer.dns_epoch.is_some() && !peer_endpoints(peer).is_empty())
            .count()
    }

    fn peer_snapshot(&self) -> Vec<ClaimPeer> {
        self.peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Stop the supervisor and close its listener and channels.
    pub async fn shutdown(&self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(Command::Shutdown(reply_tx))
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    async fn send_to_peer(&self, peer_instance_id: &str, frame: &ClaimFrame) {
        let peer = self
            .peer_snapshot()
            .iter()
            .find(|peer| peer.instance_id == peer_instance_id)
            .cloned();
        if let Some(peer) = peer {
            let _ = send_to_peer(&peer, frame).await;
        }
    }
}

impl Supervisor {
    async fn run(mut self) {
        let semaphore = Arc::new(Semaphore::new(MAX_PREAUTH_CONNECTIONS));
        let mut tasks = JoinSet::new();
        let mut rates = HashMap::<IpAddr, VecDeque<Instant>>::new();
        let _ = self.reconcile_listener().await;
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    if self.handle_command(command).await {
                        break;
                    }
                }
                changed = self.peer_watch.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.refresh_peers();
                    let _ = self.reconcile_listener().await;
                }
                accepted = accept_if_bound(self.listener.as_ref()) => {
                    if let Some((stream, address)) = accepted {
                        self.accept_connection(stream, address, &semaphore, &mut rates, &mut tasks);
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        self.stop_listener();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    async fn handle_command(&mut self, command: Option<Command>) -> bool {
        match command {
            Some(Command::Ensure(reply)) => {
                if !self.enabled {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "coordination is disabled",
                    )));
                    return false;
                }
                self.provisional_hold = true;
                let result = self.ensure_bound().await;
                let _ = reply.send(result);
                false
            }
            Some(Command::Release) => {
                self.provisional_hold = false;
                let _ = self.reconcile_listener().await;
                false
            }
            Some(Command::Update {
                enabled,
                address,
                port,
                reply,
            }) => {
                let result = self.update_config(enabled, address, port).await;
                let _ = reply.send(result);
                false
            }
            Some(Command::Shutdown(reply)) => {
                let _ = reply.send(());
                true
            }
            None => true,
        }
    }

    fn accept_connection(
        &self,
        stream: TcpStream,
        address: SocketAddr,
        semaphore: &Arc<Semaphore>,
        rates: &mut HashMap<IpAddr, VecDeque<Instant>>,
        tasks: &mut JoinSet<()>,
    ) {
        if !allow_ip(rates, address.ip()) {
            return;
        }
        let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() else {
            return;
        };
        let context = ConnectionContext {
            identity: Arc::clone(&self.identity),
            boot_epoch: self.boot_epoch.clone(),
            peers: Arc::clone(&self.peers),
            inbound: self.inbound.clone(),
            on_peer_addr: Arc::clone(&self.on_peer_addr),
            replay: Arc::clone(&self.replay),
        };
        tasks.spawn(async move {
            let _permit = permit;
            authenticate_connection(stream, address, context).await;
        });
    }

    fn refresh_peers(&self) {
        self.peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&self.peer_watch.borrow());
    }

    fn has_peers(&self) -> bool {
        !self
            .peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    async fn reconcile_listener(&mut self) -> io::Result<()> {
        if self.enabled && (self.has_peers() || self.provisional_hold) {
            self.ensure_bound().await.map(|_| ())
        } else {
            self.stop_listener();
            Ok(())
        }
    }

    async fn ensure_bound(&mut self) -> io::Result<u16> {
        if let Some(listener) = &self.listener {
            return listener.local_addr().map(|address| address.port());
        }
        let listener = bind_listener(self.bind_address, self.fixed_port).await?;
        let port = listener.local_addr()?.port();
        self.listener = Some(listener);
        self.port.store(port, Ordering::Release);
        self.listener_port.send_replace(Some(port));
        tracing::info!(event = "claim_listener_started", port);
        Ok(port)
    }

    async fn update_config(
        &mut self,
        enabled: bool,
        address: IpAddr,
        port: Option<u16>,
    ) -> io::Result<()> {
        if self.enabled == enabled && self.bind_address == address && self.fixed_port == port {
            return Ok(());
        }
        if enabled && self.listener.is_some() {
            let replacement = bind_listener(address, port).await?;
            let replacement_port = replacement.local_addr()?.port();
            self.listener = Some(replacement);
            self.port.store(replacement_port, Ordering::Release);
            self.listener_port.send_replace(Some(replacement_port));
            tracing::info!(event = "claim_listener_started", port = replacement_port);
        }
        self.bind_address = address;
        self.fixed_port = port;
        self.enabled = enabled;
        self.reconcile_listener().await
    }

    fn stop_listener(&mut self) {
        if self.listener.take().is_some() {
            self.port.store(0, Ordering::Release);
            self.listener_port.send_replace(None);
            tracing::info!(event = "claim_listener_stopped");
        }
    }
}

struct ConnectionContext {
    identity: Arc<InstanceIdentity>,
    boot_epoch: Epoch,
    peers: Arc<RwLock<Vec<ClaimPeer>>>,
    inbound: mpsc::Sender<ClaimFrame>,
    on_peer_addr: Arc<dyn Fn(String, SocketAddr) + Send + Sync>,
    replay: Arc<Mutex<HashMap<(String, String), ReplayWindow>>>,
}

async fn authenticate_connection(
    mut stream: TcpStream,
    address: SocketAddr,
    context: ConnectionContext,
) {
    let frame =
        match tokio::time::timeout(READ_TIMEOUT, read_frame::<ClaimFrame, _>(&mut stream)).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(_)) => {
                reject("invalid_frame");
                return;
            }
            Err(_) => {
                reject("read_timeout");
                return;
            }
        };
    let peer = {
        context
            .peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|peer| peer.instance_id == frame.sender_instance_id)
            .cloned()
    };
    let Some(peer) = peer else {
        reject("unknown_peer");
        return;
    };
    let record = peer_record(&peer);
    if let Err(error) = frame.verify(
        &record,
        &context.identity.instance_id,
        context.boot_epoch.as_str(),
    ) {
        tracing::warn!(event = "claim_frame_rejected", reason = %error);
        return;
    }
    let fresh = context
        .replay
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry((peer.instance_id.clone(), frame.sender_epoch.clone()))
        .or_insert_with(|| ReplayWindow::new(64))
        .accept_frame(frame.counter, &frame.nonce);
    if !fresh {
        reject("stale_counter_or_nonce");
        return;
    }
    (context.on_peer_addr)(peer.instance_id, address);
    let _ = context.inbound.send(frame).await;
}

fn reject(reason: &'static str) {
    tracing::warn!(event = "claim_frame_rejected", reason);
}

fn peer_record(peer: &ClaimPeer) -> PeerRecord {
    PeerRecord {
        instance_id: peer.instance_id.clone(),
        ed25519_pub: STANDARD.encode(peer.verifying_key.as_bytes()),
        display_name: String::new(),
        paired_at: String::new(),
        last_addr: peer.last_addr,
        claim_port: peer.claim_port,
    }
}

fn normalize_port(port: Option<u16>) -> Option<u16> {
    port.filter(|port| *port != 0)
}

async fn bind_listener(address: IpAddr, port: Option<u16>) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(address, port.unwrap_or(0))).await
}

async fn accept_if_bound(listener: Option<&TcpListener>) -> Option<(TcpStream, SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await.ok(),
        None => std::future::pending().await,
    }
}

fn allow_ip(rates: &mut HashMap<IpAddr, VecDeque<Instant>>, ip: IpAddr) -> bool {
    let now = Instant::now();
    rates.retain(|_, entries| {
        while entries
            .front()
            .is_some_and(|seen| now.duration_since(*seen) >= Duration::from_secs(60))
        {
            entries.pop_front();
        }
        !entries.is_empty()
    });
    let entries = rates.entry(ip).or_default();
    if entries.len() >= MAX_CONNECTIONS_PER_IP_MINUTE {
        return false;
    }
    entries.push_back(now);
    true
}

fn peer_endpoints(peer: &ClaimPeer) -> Vec<(EndpointKind, SocketAddr)> {
    let mut endpoints = Vec::new();
    // Verified endpoint: requires BOTH verified address AND verified port.
    if let (Some(port), Some(address)) = (peer.claim_port, peer.last_addr) {
        endpoints.push((EndpointKind::Verified, SocketAddr::new(address.ip(), port)));
    }
    // DNS endpoint: claim_port is authoritative if present; fall back to advisory dns_port.
    let dns_port = peer.claim_port.or(peer.dns_port);
    if let (Some(port), Some(address)) = (dns_port, peer.dns_addr) {
        endpoints.push((EndpointKind::Dns, SocketAddr::new(address.ip(), port)));
    }
    endpoints
}

async fn send_to_peer(peer: &ClaimPeer, frame: &ClaimFrame) -> io::Result<()> {
    let mut last_error = None;
    for (kind, address) in peer_endpoints(peer) {
        log_dial_endpoint(kind, &peer.instance_id, address);
        match send_frame(address, frame).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "peer has no endpoint")))
}

fn log_dial_endpoint(kind: EndpointKind, peer: &str, endpoint: SocketAddr) {
    match kind {
        EndpointKind::Verified => {
            tracing::debug!(event = "claim_dial_endpoint", kind = kind.as_str(), %peer, %endpoint);
        }
        EndpointKind::Dns => {
            tracing::info!(event = "claim_dial_endpoint", kind = kind.as_str(), %peer, %endpoint);
        }
    }
}

async fn send_frame(address: SocketAddr, frame: &ClaimFrame) -> io::Result<()> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "claim peer connect timed out"))??;
    write_frame(&mut stream, frame)
        .await
        .map_err(|error| io::Error::other(format!("claim frame write failed: {error:?}")))
}

fn supervisor_stopped() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "claim transport supervisor stopped",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use dormant_core::{
        claim::{ClaimAbort, ClaimFrame, ClaimMessage, Epoch},
        peers::{InstanceIdentity, PeerRecord, instance_id_from_public_key},
    };
    use ed25519_dalek::SigningKey;
    use tokio::{
        io::AsyncReadExt as _,
        net::{TcpListener, TcpStream},
        sync::watch,
    };

    use crate::coordination_frame::{read_frame, write_frame};

    use super::{
        ClaimPeer, ClaimTransportDeps, EndpointKind, PeerStoreFeed, allow_ip, peer_endpoints, spawn,
    };

    const LOCAL_EPOCH: &str = "local-epoch-0001";
    const REMOTE_EPOCH: &str = "remote-epoch-001";

    fn identity(seed: u8) -> Arc<InstanceIdentity> {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let verifying_key = signing_key.verifying_key();
        Arc::new(InstanceIdentity {
            instance_id: instance_id_from_public_key(&verifying_key.to_bytes()),
            signing_key,
            verifying_key,
        })
    }

    fn peer(identity: &InstanceIdentity, port: Option<u16>) -> ClaimPeer {
        ClaimPeer {
            instance_id: identity.instance_id.clone(),
            verifying_key: identity.verifying_key,
            last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 9))),
            dns_addr: None,
            claim_port: port,
            dns_port: None,
            dns_epoch: None,
        }
    }

    fn signed_frame(
        sender: &InstanceIdentity,
        recipient: &InstanceIdentity,
        counter: u64,
    ) -> ClaimFrame {
        ClaimFrame::sign(
            sender,
            REMOTE_EPOCH.to_owned(),
            recipient.instance_id.clone(),
            LOCAL_EPOCH.to_owned(),
            counter,
            format!("nonce-{counter}"),
            ClaimMessage::ClaimAbort(ClaimAbort {
                nonce: format!("request-{counter}"),
            }),
        )
        .unwrap()
    }

    fn deps(
        identity: Arc<InstanceIdentity>,
        peers: watch::Receiver<Vec<ClaimPeer>>,
        calls: Arc<AtomicUsize>,
    ) -> ClaimTransportDeps {
        ClaimTransportDeps {
            identity,
            boot_epoch: Epoch::try_from(LOCAL_EPOCH).unwrap(),
            peers,
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            fixed_port: None,
            enabled: true,
            on_peer_addr: Box::new(move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
            }),
        }
    }

    async fn wait_for_port(handle: &super::ClaimTransportHandle) -> u16 {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(port) = handle.provisional_port() {
                    return port;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_parked(handle: &super::ClaimTransportHandle) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.provisional_port().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unknown_or_unsigned_peer_cannot_touch_handler_state() {
        let local = identity(1);
        let stranger = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(vec![peer(&stranger, None)]);
        let handle = spawn(deps(local.clone(), peers_rx, Arc::clone(&calls)));
        let mut inbound = handle.inbound();
        let port = wait_for_port(&handle).await;
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let unknown = identity(3);
        write_frame(&mut stream, &signed_frame(&unknown, &local, 1))
            .await
            .unwrap();
        let mut unsigned = signed_frame(&stranger, &local, 2);
        unsigned.signature.clear();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        write_frame(&mut stream, &unsigned).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), inbound.recv())
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn parked_when_empty_and_binds_on_first_peer() {
        let local = identity(1);
        let remote = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let (peers_tx, peers_rx) = watch::channel(Vec::new());
        let handle = spawn(deps(local, peers_rx, calls));
        assert_eq!(handle.provisional_port(), None);
        peers_tx.send(vec![peer(&remote, None)]).unwrap();
        let port = wait_for_port(&handle).await;
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
        );
        peers_tx.send(Vec::new()).unwrap();
        wait_for_parked(&handle).await;
        assert_eq!(handle.provisional_port(), None);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_and_release_provisional_obey_empty_peer_snapshot() {
        let local = identity(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(Vec::new());
        let handle = spawn(deps(local, peers_rx, calls));
        let port = handle.ensure_provisional_listener().await.unwrap();
        assert_eq!(handle.ensure_provisional_listener().await.unwrap(), port);
        handle.release_provisional().await;
        wait_for_parked(&handle).await;
        assert_eq!(handle.provisional_port(), None);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn releasing_provisional_keeps_listener_for_existing_peer() {
        let local = identity(1);
        let remote = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(vec![peer(&remote, None)]);
        let handle = spawn(deps(local, peers_rx, calls));
        let port = handle.ensure_provisional_listener().await.unwrap();
        handle.release_provisional().await;
        tokio::task::yield_now().await;
        assert_eq!(handle.provisional_port(), Some(port));
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn update_bind_swaps_listener_atomically() {
        let local = identity(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(Vec::new());
        let handle = spawn(deps(local, peers_rx, calls));
        let old_port = handle.ensure_provisional_listener().await.unwrap();
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let requested_port = reservation.local_addr().unwrap().port();
        drop(reservation);
        handle
            .update_config(true, IpAddr::V4(Ipv4Addr::LOCALHOST), Some(requested_port))
            .await
            .unwrap();
        let new_port = handle.provisional_port().unwrap();
        assert_ne!(old_port, new_port);
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, new_port))
                .await
                .is_ok()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn enabled_toggle_parks_and_rebinds_a_seeded_peer_listener() {
        let local = identity(1);
        let remote = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(vec![peer(&remote, None)]);
        let mut deps = deps(local, peers_rx, calls);
        deps.enabled = false;
        let handle = spawn(deps);
        assert_eq!(handle.provisional_port(), None);

        handle
            .update_config(true, IpAddr::V4(Ipv4Addr::LOCALHOST), None)
            .await
            .unwrap();
        wait_for_port(&handle).await;
        handle
            .update_config(false, IpAddr::V4(Ipv4Addr::UNSPECIFIED), None)
            .await
            .unwrap();
        wait_for_parked(&handle).await;
        handle.shutdown().await;
    }

    #[test]
    fn dns_address_is_advisory_and_verified_address_dials_first() {
        let remote = identity(2);
        let verified = SocketAddr::from(([127, 0, 0, 1], 10));
        let poisoned_dns = SocketAddr::from(([192, 0, 2, 9], 20));
        let peer = ClaimPeer {
            instance_id: remote.instance_id.clone(),
            verifying_key: remote.verifying_key,
            last_addr: Some(verified),
            dns_addr: Some(poisoned_dns),
            claim_port: Some(1234),
            dns_port: None,
            dns_epoch: None,
        };

        assert_eq!(
            peer_endpoints(&peer),
            vec![
                (
                    EndpointKind::Verified,
                    SocketAddr::from(([127, 0, 0, 1], 1234))
                ),
                (EndpointKind::Dns, SocketAddr::from(([192, 0, 2, 9], 1234))),
            ]
        );
    }

    /// A peer whose port is known only via mDNS discovery
    /// yields a usable DNS endpoint from
    /// `peer_endpoints` — the negotiated claim path must work
    /// even when no authenticated connection has refreshed
    /// the durable verified `claim_port`.
    ///
    /// **Mutation:** drop the discovered port → this test fails.
    #[test]
    fn dns_only_port_yields_dns_endpoint() {
        let remote = identity(2);
        let peer = ClaimPeer {
            instance_id: remote.instance_id.clone(),
            verifying_key: remote.verifying_key,
            last_addr: None,
            dns_addr: Some(SocketAddr::from(([10, 1, 1, 1], 4321))),
            claim_port: None, // never refreshed by verified path
            dns_port: Some(4321),
            dns_epoch: None,
        };

        assert_eq!(
            peer_endpoints(&peer),
            vec![(EndpointKind::Dns, SocketAddr::from(([10, 1, 1, 1], 4321)))]
        );

        // Mutation check — drop dns_port, endpoints must be empty
        let mut mutated = peer.clone();
        mutated.dns_port = None;
        assert!(
            peer_endpoints(&mutated).is_empty(),
            "without dns_port, peer_endpoints must be empty when claim_port is also None"
        );
    }

    /// A peer with `last_addr` + `dns_port` (but NO `claim_port`)
    /// must NOT produce a Verified endpoint — only
    /// `claim_port` (authenticated) licenses the Verified kind.
    ///
    /// **Security invariant:** unauthenticated mDNS data
    /// (`dns_port`) never gates a Verified endpoint.
    #[test]
    fn dns_port_never_yields_verified_endpoint() {
        let remote = identity(2);
        let peer = ClaimPeer {
            instance_id: remote.instance_id.clone(),
            verifying_key: remote.verifying_key,
            last_addr: Some(SocketAddr::from(([127, 0, 0, 1], 10))),
            dns_addr: Some(SocketAddr::from(([10, 1, 1, 1], 4321))),
            claim_port: None,
            dns_port: Some(4321),
            dns_epoch: None,
        };

        let eps = peer_endpoints(&peer);
        // Only one endpoint (DNS). No Verified — dns_port does
        // not qualify for the verified kind.
        assert_eq!(eps.len(), 1);
        assert!(
            eps.iter()
                .all(|(kind, _)| matches!(kind, EndpointKind::Dns)),
            "dns_port alone must not produce a Verified endpoint"
        );
        // The endpoint uses the DNS address with the DNS port.
        assert_eq!(
            eps[0],
            (EndpointKind::Dns, SocketAddr::from(([10, 1, 1, 1], 4321)))
        );
    }

    /// `claim_port` (verified) is authoritative — when present
    /// it applies to BOTH `last_addr` and `dns_addr` endpoints,
    /// shadowing the advisory `dns_port`.
    #[test]
    fn verified_claim_port_shadows_dns_port() {
        let remote = identity(2);
        let peer = ClaimPeer {
            instance_id: remote.instance_id.clone(),
            verifying_key: remote.verifying_key,
            last_addr: Some(SocketAddr::from(([127, 0, 0, 1], 10))),
            dns_addr: Some(SocketAddr::from(([10, 1, 1, 1], 4321))),
            claim_port: Some(9999),
            dns_port: Some(4321),
            dns_epoch: None,
        };

        let eps = peer_endpoints(&peer);
        // Both endpoints use claim_port (9999), not dns_port (4321).
        assert_eq!(
            eps,
            vec![
                (
                    EndpointKind::Verified,
                    SocketAddr::from(([127, 0, 0, 1], 9999))
                ),
                (EndpointKind::Dns, SocketAddr::from(([10, 1, 1, 1], 9999))),
            ]
        );
    }

    #[test]
    fn rate_entries_expire_under_many_ip_spray() {
        let stale = Instant::now().checked_sub(Duration::from_secs(61)).unwrap();
        let mut rates = HashMap::new();
        for octet in 1..=100 {
            rates.insert(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, octet)),
                VecDeque::from([stale]),
            );
        }

        assert!(allow_ip(&mut rates, IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(rates.len(), 1);
    }

    #[tokio::test]
    async fn fifth_concurrent_pre_auth_connection_is_rejected() {
        let local = identity(1);
        let remote = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(vec![peer(&remote, None)]);
        let handle = spawn(deps(local, peers_rx, calls));
        let port = wait_for_port(&handle).await;
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(
                TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap(),
            );
        }
        let mut fifth = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(200), fifth.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0);
        drop(held);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn authenticated_inbound_reaches_channel_and_refreshes_real_remote_addr() {
        let local = identity(1);
        let remote = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(std::sync::Mutex::new(None));
        let (_peers_tx, peers_rx) = watch::channel(vec![peer(&remote, None)]);
        let observed_callback = Arc::clone(&observed);
        let calls_callback = Arc::clone(&calls);
        let handle = spawn(ClaimTransportDeps {
            identity: local.clone(),
            boot_epoch: Epoch::try_from(LOCAL_EPOCH).unwrap(),
            peers: peers_rx,
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            fixed_port: None,
            enabled: true,
            on_peer_addr: Box::new(move |_, address| {
                calls_callback.fetch_add(1, Ordering::SeqCst);
                *observed_callback.lock().unwrap() = Some(address);
            }),
        });
        let mut inbound = handle.inbound();
        let port = wait_for_port(&handle).await;
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let local_addr = stream.local_addr().unwrap();
        write_frame(&mut stream, &signed_frame(&remote, &local, 1))
            .await
            .unwrap();
        let received = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.sender_instance_id, remote.instance_id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*observed.lock().unwrap(), Some(local_addr));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn fanout_result_counts_only_successful_frame_writes() {
        let local = identity(1);
        let reachable = identity(2);
        let unreachable = identity(3);
        let missing_epoch = identity(4);
        let calls = Arc::new(AtomicUsize::new(0));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let reachable_port = listener.local_addr().unwrap().port();
        let closed_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let unreachable_port = closed_listener.local_addr().unwrap().port();
        drop(closed_listener);

        let (_peers_tx, peers_rx) = watch::channel(vec![
            ClaimPeer {
                instance_id: reachable.instance_id.clone(),
                verifying_key: reachable.verifying_key,
                last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                dns_addr: None,
                claim_port: Some(reachable_port),
                dns_port: None,
                dns_epoch: Some(Epoch::try_from("peer-a-epoch-001").unwrap()),
            },
            ClaimPeer {
                instance_id: unreachable.instance_id.clone(),
                verifying_key: unreachable.verifying_key,
                last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                dns_addr: None,
                claim_port: Some(unreachable_port),
                dns_port: None,
                dns_epoch: Some(Epoch::try_from("peer-b-epoch-002").unwrap()),
            },
            ClaimPeer {
                instance_id: missing_epoch.instance_id.clone(),
                verifying_key: missing_epoch.verifying_key,
                last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                dns_addr: None,
                claim_port: Some(reachable_port),
                dns_port: None,
                dns_epoch: None,
            },
        ]);
        let handle = spawn(deps(local, peers_rx, calls));
        let accept_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame::<ClaimFrame, _>(&mut stream).await.unwrap()
        });

        let result = handle
            .fanout_request(
                1,
                "count-nonce",
                &ClaimMessage::ClaimAbort(ClaimAbort {
                    nonce: "count-request".to_owned(),
                }),
            )
            .await;

        assert_eq!(result.contacted, 1);
        assert_eq!(result.dial_failed, 1);
        assert_eq!(result.skipped_no_epoch, 1);
        tokio::time::timeout(Duration::from_secs(2), accept_task)
            .await
            .expect("reachable peer receives frame")
            .unwrap();
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn unreachable_peer_dial_is_bounded() {
        let local = identity(1);
        let remote = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_peers_tx, peers_rx) = watch::channel(vec![ClaimPeer {
            instance_id: remote.instance_id.clone(),
            verifying_key: remote.verifying_key,
            last_addr: Some(SocketAddr::from(([192, 0, 2, 1], 9))),
            dns_addr: None,
            claim_port: Some(65_000),
            dns_port: None,
            dns_epoch: Some(Epoch::try_from(REMOTE_EPOCH).unwrap()),
        }]);
        let handle = spawn(deps(local.clone(), peers_rx, calls));
        let started = tokio::time::Instant::now();
        handle
            .fanout_request(
                1,
                "nonce-1",
                &ClaimMessage::ClaimAbort(ClaimAbort {
                    nonce: "request-1".to_owned(),
                }),
            )
            .await;
        assert!(started.elapsed() <= Duration::from_millis(700));
        handle.shutdown().await;
    }

    /// `fanout_request` must sign each frame with the target peer's instance id
    /// and current boot epoch — never the wildcard `"*"` or the sender's own epoch.
    ///
    /// **Regression guard for Bug C/D in commit `7b7f84b`:** reverting to the
    /// old broadcast pattern (`"*"` recipient / sender epoch) must fail this test.
    #[tokio::test]
    async fn fanout_request_sends_per_peer_addressed_frames() {
        let local = identity(1);
        let peer_a = identity(2);
        let peer_b = identity(3);
        let calls = Arc::new(AtomicUsize::new(0));

        let epoch_a = "peer-a-epoch-001";
        let epoch_b = "peer-b-epoch-002";

        let listener_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        let port_b = listener_b.local_addr().unwrap().port();

        let (_peers_tx, peers_rx) = watch::channel(vec![
            ClaimPeer {
                instance_id: peer_a.instance_id.clone(),
                verifying_key: peer_a.verifying_key,
                last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                dns_addr: None,
                claim_port: Some(port_a),
                dns_port: None,
                dns_epoch: Some(Epoch::try_from(epoch_a).unwrap()),
            },
            ClaimPeer {
                instance_id: peer_b.instance_id.clone(),
                verifying_key: peer_b.verifying_key,
                last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                dns_addr: None,
                claim_port: Some(port_b),
                dns_port: None,
                dns_epoch: Some(Epoch::try_from(epoch_b).unwrap()),
            },
        ]);

        let handle = spawn(deps(local.clone(), peers_rx, Arc::clone(&calls)));

        // Accept tasks must be spawned before fanout so listeners are ready.
        let a_task = tokio::spawn(async move {
            let (mut stream, _) = listener_a.accept().await.unwrap();
            read_frame::<ClaimFrame, _>(&mut stream).await.unwrap()
        });
        let b_task = tokio::spawn(async move {
            let (mut stream, _) = listener_b.accept().await.unwrap();
            read_frame::<ClaimFrame, _>(&mut stream).await.unwrap()
        });

        handle
            .fanout_request(
                1,
                "nonce",
                &ClaimMessage::ClaimAbort(ClaimAbort {
                    nonce: "request".to_owned(),
                }),
            )
            .await;

        let frame_a = tokio::time::timeout(Duration::from_secs(2), a_task)
            .await
            .unwrap()
            .unwrap();
        let frame_b = tokio::time::timeout(Duration::from_secs(2), b_task)
            .await
            .unwrap()
            .unwrap();

        // Each peer receives a frame addressed to its own identity.
        assert_eq!(frame_a.recipient_instance_id, peer_a.instance_id);
        assert_eq!(frame_a.recipient_epoch, epoch_a);
        assert_eq!(frame_b.recipient_instance_id, peer_b.instance_id);
        assert_eq!(frame_b.recipient_epoch, epoch_b);

        // Neither frame carries the wildcard or the sender's own epoch.
        assert_ne!(frame_a.recipient_instance_id, "*");
        assert_ne!(frame_a.recipient_epoch, LOCAL_EPOCH);
        assert_ne!(frame_b.recipient_instance_id, "*");
        assert_ne!(frame_b.recipient_epoch, LOCAL_EPOCH);

        // The two frames carry different recipient fields.
        assert_ne!(frame_a.recipient_instance_id, frame_b.recipient_instance_id);
        assert_ne!(frame_a.recipient_epoch, frame_b.recipient_epoch);

        handle.shutdown().await;
    }

    /// A peer whose `dns_epoch` is `None` must be skipped by `fanout_request` —
    /// no dial attempt, no frame sent. The `claim_peer_no_epoch` log anchor
    /// covers the skip; this test proves it at the transport level.
    ///
    /// **Regression guard:** removing the `dns_epoch.is_none()` continue must
    /// fail this test.
    #[tokio::test]
    async fn fanout_request_skips_peer_without_epoch() {
        let local = identity(1);
        let stale_peer = identity(2);
        let calls = Arc::new(AtomicUsize::new(0));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let (_peers_tx, peers_rx) = watch::channel(vec![ClaimPeer {
            instance_id: stale_peer.instance_id.clone(),
            verifying_key: stale_peer.verifying_key,
            last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            dns_addr: None,
            claim_port: Some(port),
            dns_port: None,
            dns_epoch: None, // must be skipped
        }]);

        let handle = spawn(deps(local.clone(), peers_rx, Arc::clone(&calls)));

        let accept_task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(200), listener.accept()).await
        });

        handle
            .fanout_request(
                1,
                "nonce",
                &ClaimMessage::ClaimAbort(ClaimAbort {
                    nonce: "request".to_owned(),
                }),
            )
            .await;

        let result = accept_task.await.unwrap();
        assert!(
            result.is_err(),
            "fanout_request must not dial a peer with dns_epoch = None"
        );

        handle.shutdown().await;
    }

    #[test]
    fn peer_store_feed_publishes_pairing_and_authenticated_endpoint_changes() {
        let state = tempfile::tempdir().unwrap();
        let remote = identity(2);
        let feed = PeerStoreFeed::load(state.path()).unwrap();
        let mut peers = feed.subscribe();
        let record = PeerRecord {
            instance_id: remote.instance_id.clone(),
            ed25519_pub: STANDARD.encode(remote.verifying_key.as_bytes()),
            display_name: "remote".to_owned(),
            paired_at: "2026-01-01T00:00:00Z".to_owned(),
            last_addr: None,
            claim_port: Some(9),
        };

        feed.upsert(record).unwrap();
        assert!(peers.has_changed().unwrap());
        peers.borrow_and_update();
        feed.refresh_verified_address(&remote.instance_id, SocketAddr::from(([127, 0, 0, 1], 10)))
            .unwrap();

        assert!(peers.has_changed().unwrap());
        assert_eq!(
            peers.borrow_and_update()[0].last_addr,
            Some(SocketAddr::from(([127, 0, 0, 1], 10)))
        );
        feed.refresh_dns_address(
            &remote.instance_id,
            SocketAddr::from(([192, 0, 2, 9], 20)),
            None,
        );
        assert_eq!(
            peers.borrow_and_update()[0].last_addr,
            Some(SocketAddr::from(([127, 0, 0, 1], 10)))
        );
        assert_eq!(
            peers.borrow()[0].dns_addr,
            Some(SocketAddr::from(([192, 0, 2, 9], 20)))
        );
    }
}
