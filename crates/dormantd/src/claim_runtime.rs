//! KVM claim runtime — the integration layer between the pure
//! [`dormant_core::claim_engine::ClaimEngine`], the authenticated
//! [`crate::coordination_claim::ClaimTransportHandle`], the four-slot hook
//! engine, and the per-display [`dormant_core::traits::CommandSink`].
//!
//! See the module docstring on `lib.rs` for the high-level design; this
//! file is the driver. The pure [`ClaimEngine`] is intentionally agnostic
//! to network, hook, and display I/O — the driver maps its `Action`
//! stream onto the daemon's I/O surfaces and back into the engine's
//! `RequesterEvent` / `OwnerEvent` feed.
//!
//! ## Concurrency model
//!
//! A single async task owns the per-display `ClaimEngine` instances
//! and the per-display active-flight side tables (the pure engine
//! doesn't expose the in-flight nonce or peer instance id; the
//! driver carries them). The task `select!`s on the transport's
//! inbound-frame channel, a control channel carrying ownership
//! and IPC events (from the existing coordination poller — F10
//! suppression and requester flip detection — and from the IPC
//! surface), a periodic 100 ms tick driving the engine's
//! deadline sweep, and the daemon-lifetime cancellation token.
//!
//! Hook / write / wake actions spawn short-lived `tokio::spawn`
//! tasks so the driver's main loop stays responsive; the hook
//! outcome is fed back to the engine as an `OwnerEvent`.
// Allow pedantic lints for this new integration layer: the
// driver is intentionally side-effecting and the integration
// surface is wider than the pure engine's.
#![allow(
    clippy::too_many_arguments,
    clippy::collapsible_if,
    clippy::bind_instead_of_map,
    clippy::useless_conversion,
    clippy::needless_borrow,
    clippy::needless_pass_by_value
)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dormant_core::claim::{
    ClaimAbort, ClaimDeniedReason, ClaimFrame, ClaimMessage, ClaimRequest, ClaimResponse,
    ClaimVerdict, ReleaseFailed,
};
use dormant_core::claim_engine::{
    Action, ClaimCapability, ClaimEngine, ClaimFailure, HookResult, OwnerDisposition, OwnerEvent,
    OwnerRequest, RequesterEvent,
};
use dormant_core::config::Config;
use dormant_core::config::schema::{ActivityClaimPolicy, HookAction, HookSlots, KeymapConfig};
use dormant_core::coordination::CoordinationHandle;
use dormant_core::peers::InstanceIdentity;
use dormant_core::traits::{CommandSink, InputSourceReadback, InputSourceTarget};
use dormant_core::types::DisplayId;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::coordination_claim::{ClaimTransportHandle, FanoutResult};
use crate::hooks::{Direction, HookContext, HookEngine, HookOutcome, HookSlot, Phase};

/// Literal claim lifecycle anchors. Re-exported from the pure
/// engine as the single source of truth.
pub use dormant_core::claim_engine::CLAIM_EVENTS;

/// Deadline-sweep period (100 ms — tight enough for a smooth UX,
/// loose enough to keep the busy loop cold).
const DEADLINE_SWEEP_PERIOD: Duration = Duration::from_millis(100);

fn append_event(
    event_log: Option<&Arc<Mutex<Vec<String>>>>,
    event_notify: Option<&Arc<Notify>>,
    event: impl Into<String>,
) {
    let appended = event_log.is_some_and(|log| {
        log.lock().is_ok_and(|mut events| {
            events.push(event.into());
            true
        })
    });
    if appended {
        if let Some(notify) = event_notify {
            notify.notify_one();
        }
    }
}

/// Outcome of an input-source writability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputWritability {
    /// The controller successfully read and wrote the input source —
    /// it is definitively writable.
    Capable,
    /// The controller's chain-walk returned
    /// `INPUT_SOURCE_WRITE_UNSUPPORTED` — the display cannot
    /// participate in input-source claims.
    Incapable,
    /// The probe could not complete: a sampler-priority read skipped
    /// (yielded to a concurrent command-path transaction), a transient
    /// I/O error, or no controller reported a code. The capability is
    /// not known — re-probe when the next generation refreshes (the
    /// real claim path uses command priority and completes even when
    /// this probe is inconclusive).
    Unknown,
}

impl InputWritability {
    /// `true` when the display is NOT definitively incapable —
    /// i.e. claims should be attempted (`Capable` or `Unknown`).
    fn is_not_incapable(self) -> bool {
        !matches!(self, Self::Incapable)
    }
}

/// Per-display generation-stable facts.
#[derive(Clone)]
struct DisplayCtx {
    /// Local configured input-source code (the `0x60` value this
    /// daemon expects to see for "owned"). `None` ⇒ the display
    /// cannot participate in claims.
    ///
    /// This is the READ code — what the panel reports on VCP 0x60 when
    /// this input is active.  The ownership poll compares observations
    /// against this value.
    local_input_code: Option<u8>,
    /// Input code to WRITE when selecting this machine's input.
    /// Defaults to [`local_input_code`] when absent; only set when the
    /// panel accepts a different code on the write path than it reports
    /// on the read path.
    local_input_write_code: Option<u8>,
    /// Cross-machine claim identity (F5: `manufacturer:model[:serial]`).
    claim_identity: Option<String>,
    /// Whether the chained controller exposes an input-source
    /// write surface. `true` for `Capable` or `Unknown`;
    /// `false` only when the controller definitively returned
    /// `INPUT_SOURCE_WRITE_UNSUPPORTED`.
    writable: bool,
    /// Generation-stable hook snapshot.
    hooks: Arc<HookSlots>,
}

impl DisplayCtx {
    /// Effective code to write when selecting this machine's input.
    /// Falls back to the read code when no explicit write-override is set.
    fn effective_write_code(&self) -> Option<u8> {
        self.local_input_write_code.or(self.local_input_code)
    }
}

/// Per-display live flight tracking. The pure engine doesn't
/// carry the in-flight nonce or peer instance id; the driver
/// tracks them so outbound sends know who to address and which
/// nonce to copy.
#[derive(Clone)]
struct ActiveFlight {
    /// The in-flight nonce (request side: the request's nonce;
    /// owner side: copied onto the verdict and the release-failed
    /// notifications).
    nonce: String,
    /// The peer instance id for the other side of this flight
    /// (owner: the inbound requester; requester: the inbound
    /// Accepted verdict sender).
    peer_instance_id: String,
    /// The code the OWNER side must select when it writes the
    /// input. For the requester side this is the LOCAL code
    /// (the fallback writes the local code). Stored explicitly
    /// so the driver's `write_input_source` action can pick the
    /// right code per side without re-deriving it from the
    /// engine's `Action::WriteInput` (which is unparameterized).
    write_code: u8,
    /// Epoch authenticated in the peer's signed frame. This is the
    /// recipient epoch for replies; it is distinct from the local
    /// transport epoch and from the advisory mDNS epoch.
    peer_epoch: String,
}

/// Sign a reply for a peer using the epoch authenticated in its inbound frame.
/// The verified peer epoch is not interchangeable with this daemon's local
/// transport epoch or with an advisory mDNS epoch.
fn sign_frame_for_peer(
    identity: &InstanceIdentity,
    sender_epoch: String,
    peer_instance_id: String,
    peer_epoch: String,
    counter: u64,
    nonce: String,
    message: ClaimMessage,
) -> Result<ClaimFrame, dormant_core::claim::ClaimFrameError> {
    ClaimFrame::sign(
        identity,
        sender_epoch,
        peer_instance_id,
        peer_epoch,
        counter,
        nonce,
        message,
    )
}

fn negotiated_peer_count(result: FanoutResult) -> Option<usize> {
    (result.contacted > 0).then_some(result.contacted)
}

/// Runtime events consumed by the driver.
#[allow(
    clippy::large_enum_variant,
    reason = "Inbound carries the signed ClaimFrame (~hundreds of bytes); the other arms are the cheap IPC reply oneshots."
)]
#[allow(
    dead_code,
    reason = "Inbound is constructed in claim_listener.rs, which is #[cfg(any(test, feature = \"test-util\"))] for now."
)]
#[derive(Debug)]
enum RuntimeEvent {
    Inbound(ClaimFrame),
    ClaimShared {
        display: DisplayId,
        reply: oneshot::Sender<ClaimSharedResult>,
    },
    ClaimArm {
        display: DisplayId,
        reply: oneshot::Sender<Result<Instant, ArmFailure>>,
    },
    /// Send an `IdleQuery` to the owner and wait for the `IdleReport`.
    /// Returns the owner's idle duration in milliseconds, or `None`
    /// when `claim_timeout` elapses without a response.
    IdleQuery {
        display: DisplayId,
        reply: oneshot::Sender<Option<u64>>,
    },
    DisplayRemoved(DisplayId),
    #[cfg(any(test, feature = "test-util"))]
    InjectOwnerCompletion {
        display: DisplayId,
        nonce: String,
        event: OwnerEvent,
        reply: oneshot::Sender<bool>,
    },
    #[cfg(any(test, feature = "test-util"))]
    /// Directly resolve a pending idle query with `idle_ms`,
    /// bypassing the signed-frame transport.  Used by the
    /// `OwnerIdle` policy integration tests.
    InjectIdleReport {
        nonce: String,
        idle_ms: u64,
    },
    #[cfg(any(test, feature = "test-util"))]
    RequesterNonce {
        display: DisplayId,
        reply: oneshot::Sender<Option<String>>,
    },
}

/// Local verdict surfaced to the IPC caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimSharedResult {
    /// Owner accepted the request and committed to the release.
    Accepted {
        /// Local deadline for the negotiated handoff
        /// (`now + release_deadline`).
        deadline: Instant,
    },
    /// Another local trigger is in flight.
    Busy,
    /// Owner denied the request for a diagnosable reason.
    Denied(ClaimDeniedReason),
    /// No usable peer accepted and the fallback did not apply
    /// (e.g. panel in standby and no owner reachable).
    Failed(ClaimFailure),
}

/// Failures the `ClaimArm` IPC may surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmFailure {
    /// Coordination is disabled.
    CoordinationDisabled,
    /// Activity-claim policy is not `armed`.
    NotArmed,
    /// The display is not claim-capable.
    NotClaimCapable,
}

/// Public surface for callers that do not own the driver task.
#[derive(Clone)]
pub struct ClaimRuntimeHandle {
    cmd_tx: mpsc::Sender<RuntimeEvent>,
    /// Per-display armed-until instant.
    armed: Arc<Mutex<HashMap<DisplayId, Instant>>>,
    /// Resolved `claim_capable_displays` set, refreshed on every
    /// generation install.
    claim_capable: Arc<Mutex<Vec<DisplayId>>>,
    /// Resolved keymap (read by the snapshot).
    keymap: Arc<Mutex<KeymapConfig>>,
    /// Resolved activity-claim policy.
    activity_claim: Arc<Mutex<ActivityClaimPolicy>>,
    /// Bound `release_deadline_cap` for the snapshot.
    release_deadline_cap: Arc<Mutex<Duration>>,
    /// Per-display in-flight state for F10 suppression.
    suppressed: Arc<Mutex<HashMap<DisplayId, Instant>>>,
}

impl ClaimRuntimeHandle {
    /// Create a minimal handle for unit tests that only need the
    /// public query surface (`is_armed`, `kvm_status`, etc.) without
    /// spawning a full driver.  `try_claim` and `arm` will return
    /// transport-level errors (no driver behind the command channel).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test() -> Self {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<RuntimeEvent>(1);
        Self {
            cmd_tx,
            armed: Arc::new(Mutex::new(HashMap::new())),
            claim_capable: Arc::new(Mutex::new(Vec::new())),
            keymap: Arc::new(Mutex::new(KeymapConfig::default())),
            activity_claim: Arc::new(Mutex::new(ActivityClaimPolicy::default())),
            release_deadline_cap: Arc::new(Mutex::new(Duration::from_secs(45))),
            suppressed: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Initiate a claim on `display` from a local trigger.
    ///
    /// # Errors
    ///
    /// Returns `Err("claim runtime not available")` if the
    /// runtime's command channel is closed (the driver task
    /// has exited), and `Err("claim runtime dropped reply")`
    /// if the runtime processes the event but the reply
    /// oneshot is dropped before the driver can populate it
    /// (a panic in the dispatch path).
    pub async fn try_claim(&self, display: DisplayId) -> Result<ClaimSharedResult, &'static str> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeEvent::ClaimShared { display, reply: tx })
            .await
            .map_err(|_| "claim runtime not available")?;
        rx.await.map_err(|_| "claim runtime dropped reply")
    }

    /// Arm `display` for the `armed` activity-claim policy.
    ///
    /// # Errors
    ///
    /// Returns `Err("claim runtime not available")` if the
    /// runtime's command channel is closed, or
    /// `Err("claim runtime dropped reply")` if the dispatch
    /// path drops the reply oneshot.
    pub async fn arm(
        &self,
        display: DisplayId,
        policy: ActivityClaimPolicy,
    ) -> Result<Result<Instant, ArmFailure>, &'static str> {
        if !matches!(policy, ActivityClaimPolicy::Armed) {
            return Ok(Err(ArmFailure::NotArmed));
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeEvent::ClaimArm { display, reply: tx })
            .await
            .map_err(|_| "claim runtime not available")?;
        rx.await.map_err(|_| "claim runtime dropped reply")
    }

    /// Notify the runtime that a display disappeared from the
    /// generation. Drives `DisplayRemoved` for every active flight.
    ///
    /// # Errors
    ///
    /// Returns `Err("claim runtime not available")` if the
    /// runtime's command channel is closed.
    pub async fn display_removed(&self, display: DisplayId) -> Result<(), &'static str> {
        self.cmd_tx
            .send(RuntimeEvent::DisplayRemoved(display))
            .await
            .map_err(|_| "claim runtime not available")
    }

    /// Query the current owner's idle duration for an `owner-idle`
    /// activity-claim policy.
    ///
    /// Signs an `IdleQuery`, fans it to all peers, waits for the
    /// first `IdleReport` response (bounded by `claim_timeout`),
    /// and returns the owner's idle duration in milliseconds.
    ///
    /// Returns `None` when no response arrives before the timeout
    /// or when the runtime is unavailable.
    ///
    /// # Errors
    ///
    /// Returns `Err("claim runtime not available")` if the
    /// runtime's command channel is closed.
    pub async fn idle_query(&self, display: DisplayId) -> Result<Option<u64>, &'static str> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeEvent::IdleQuery { display, reply: tx })
            .await
            .map_err(|_| "claim runtime not available")?;
        rx.await.map_err(|_| "claim runtime dropped reply")
    }

    /// **Test seam**: inject an authenticated inbound `ClaimFrame`
    /// directly into the driver as if it had arrived via the
    /// transport. Used by the smoke tests to drive the OWNER
    /// path without the second in-process daemon the task
    /// description calls out (a `loopback` harness would be
    /// heavier; this seam exercises the same code paths).
    ///
    /// # Errors
    ///
    /// Returns `Err("claim runtime not available")` if the
    /// runtime's command channel is closed.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn inject_inbound_for_test(&self, frame: ClaimFrame) -> Result<(), &'static str> {
        self.cmd_tx
            .send(RuntimeEvent::Inbound(frame))
            .await
            .map_err(|_| "claim runtime not available")
    }

    /// **Test seam**: directly resolve a pending idle query, bypassing
    /// the signed-frame transport.  Used by the `OwnerIdle` policy
    /// integration tests.
    ///
    /// # Errors
    ///
    /// Returns `Err("claim runtime not available")` if the
    /// runtime's command channel is closed.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn inject_idle_report_for_test(
        &self,
        nonce: String,
        idle_ms: u64,
    ) -> Result<(), &'static str> {
        self.cmd_tx
            .send(RuntimeEvent::InjectIdleReport { nonce, idle_ms })
            .await
            .map_err(|_| "claim runtime not available")
    }

    /// Return the active requester nonce for an integration test.
    ///
    /// # Errors
    ///
    /// Returns an error if the driver exits before answering.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn requester_nonce_for_test(
        &self,
        display: DisplayId,
    ) -> Result<Option<String>, &'static str> {
        let (reply, received) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeEvent::RequesterNonce { display, reply })
            .await
            .map_err(|_| "claim runtime not available")?;
        received
            .await
            .map_err(|_| "claim runtime dropped acknowledgement")
    }

    /// Inject an asynchronous owner completion through the driver's event loop.
    /// Returns `true` when its nonce matches the active flight; stale completions
    /// return `false` without reaching the engine.
    ///
    /// # Errors
    ///
    /// Returns an error if the driver has exited or drops the acknowledgement.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn inject_owner_completion_for_test(
        &self,
        display: DisplayId,
        nonce: impl Into<String>,
        event: OwnerEvent,
    ) -> Result<bool, &'static str> {
        let (reply, acknowledged) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeEvent::InjectOwnerCompletion {
                display,
                nonce: nonce.into(),
                event,
                reply,
            })
            .await
            .map_err(|_| "claim runtime not available")?;
        acknowledged
            .await
            .map_err(|_| "claim runtime dropped acknowledgement")
    }

    /// F10 suppression: `true` when the runtime owns an in-flight
    /// local claim/release entry for `display` whose phase
    /// deadline has not elapsed.
    #[must_use]
    pub fn is_suppressed(&self, display: &DisplayId, now: Instant) -> bool {
        let Ok(map) = self.suppressed.lock() else {
            return false;
        };
        map.get(display).is_some_and(|deadline| now < *deadline)
    }

    /// Whether `display` is currently armed for activity claims
    /// and the arm window has not yet expired.
    #[must_use]
    pub fn is_armed(&self, display: &DisplayId) -> bool {
        self.armed_deadline(display)
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// The armed expiry deadline for `display`, or `None` when
    /// not armed or the entry has expired.
    #[must_use]
    pub fn armed_deadline(&self, display: &DisplayId) -> Option<Instant> {
        let Ok(map) = self.armed.lock() else {
            return None;
        };
        map.get(display).copied()
    }

    /// Resolve the `claim_capable_displays` set, the
    /// keymap / policy, and per-display armed-remaining-ms
    /// for the snapshot.
    #[must_use]
    pub fn kvm_status(&self) -> KvmStatus {
        let keymap = self.keymap.lock().map(|g| g.clone()).unwrap_or_default();
        let activity_claim = self.activity_claim.lock().map(|g| *g).unwrap_or_default();
        let claim_capable_displays = self
            .claim_capable
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let now = Instant::now();
        let claim_armed_remaining: Vec<(DisplayId, u64)> = self
            .armed
            .lock()
            .map(|g| {
                g.iter()
                    .filter_map(|(display, deadline)| {
                        if now < *deadline {
                            let ms = deadline.saturating_duration_since(now).as_millis();
                            Some((display.clone(), u64::try_from(ms).unwrap_or(u64::MAX)))
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        KvmStatus {
            keymap,
            claim_capable_displays,
            activity_claim,
            claim_armed_remaining,
        }
    }

    /// Refresh the per-generation resolved state. Called by the
    /// orchestrator on every successful generation install.
    pub fn refresh_status(
        &self,
        claim_capable: Vec<DisplayId>,
        keymap: KeymapConfig,
        activity_claim: ActivityClaimPolicy,
        release_deadline_cap: Duration,
    ) {
        if let Ok(mut cap) = self.claim_capable.lock() {
            *cap = claim_capable;
        }
        if let Ok(mut km) = self.keymap.lock() {
            *km = keymap;
        }
        if let Ok(mut ac) = self.activity_claim.lock() {
            *ac = activity_claim;
        }
        if let Ok(mut rc) = self.release_deadline_cap.lock() {
            *rc = release_deadline_cap;
        }
    }
}

/// Snapshot payload the tray consumes. Re-exported as
/// `dormant_core::rules::KvmStatus` for the wire/snapshot fold;
/// the runtime's `kvm_status()` method returns the canonical
/// type.
pub use dormant_core::rules::KvmStatus;

/// Full dependencies for the claim runtime driver.
pub struct ClaimRuntimeDeps {
    pub identity: Arc<InstanceIdentity>,
    pub transport: Arc<ClaimTransportHandle>,
    pub executors: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    pub config: watch::Receiver<Arc<Config>>,
    pub hooks: Arc<HookEngine>,
    pub coordination: Option<CoordinationHandle>,
    /// Daemon-front control channel (forwarded to the engine).
    /// Used to publish `ControlMsg::SetClaimSuppression` so the
    /// rules engine can gate ownership-loss reactions (F10).
    pub front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    pub cancel: CancellationToken,
    /// Test seam — an append-only log of emitted anchors.
    pub event_log: Option<Arc<Mutex<Vec<String>>>>,
    /// Signals test waiters after an anchor is appended.
    pub event_notify: Option<Arc<Notify>>,
    /// Daemon-lifetime idle-observation channel consumed by the
    /// owner-side `IdleQuery` handler — the runtime reads its
    /// LOCAL idle state to answer remote idle queries.
    pub idle_rx: Option<crate::idle_observation::IdleObservationRx>,
}

fn log_inbound_forwarder_closed(reason: &'static str) {
    warn!(
        event = "claim_inbound_forwarder_closed_unexpectedly",
        reason,
    );
}

/// Forward only frames already authenticated by the transport into the runtime.
/// Both channels are bounded (32 frames each), so a stalled runtime applies
/// backpressure instead of growing an unbounded peer-controlled queue.
async fn forward_verified_inbound(
    mut inbound: mpsc::Receiver<ClaimFrame>,
    runtime_tx: mpsc::Sender<RuntimeEvent>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            () = runtime_tx.closed() => {
                if !cancel.is_cancelled() {
                    log_inbound_forwarder_closed("runtime_channel_closed");
                }
                break;
            }
            frame = inbound.recv() => {
                let Some(frame) = frame else {
                    if !cancel.is_cancelled() {
                        log_inbound_forwarder_closed("transport_channel_closed");
                    }
                    break;
                };
                tokio::select! {
                    () = cancel.cancelled() => break,
                    result = runtime_tx.send(RuntimeEvent::Inbound(frame)) => {
                        if result.is_err() {
                            if !cancel.is_cancelled() {
                                log_inbound_forwarder_closed("runtime_channel_closed");
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Spawn the claim runtime driver. The returned handle is the
/// orchestrator's integration point (IPC, F10 queries, snapshot
/// status).
#[must_use = "the claim-runtime handle is the orchestrator's integration point"]
pub fn spawn(deps: ClaimRuntimeDeps) -> ClaimRuntimeHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<RuntimeEvent>(32);
    let inbound = deps.transport.inbound();
    let inbound_tx = cmd_tx.clone();
    let inbound_cancel = deps.cancel.clone();
    let (owner_event_tx, owner_event_rx) = mpsc::channel::<(DisplayId, String, OwnerEvent)>(64);
    let armed = Arc::new(Mutex::new(HashMap::<DisplayId, Instant>::new()));
    let claim_capable = Arc::new(Mutex::new(Vec::<DisplayId>::new()));
    let keymap = Arc::new(Mutex::new(KeymapConfig::default()));
    let activity_claim = Arc::new(Mutex::new(ActivityClaimPolicy::default()));
    let release_deadline_cap = Arc::new(Mutex::new(Duration::from_secs(45)));
    let suppressed = Arc::new(Mutex::new(HashMap::<DisplayId, Instant>::new()));
    let handle = ClaimRuntimeHandle {
        cmd_tx,
        armed,
        claim_capable,
        keymap,
        activity_claim,
        release_deadline_cap,
        suppressed,
    };
    let driver_handle = handle.clone();
    let driver = Driver {
        engines: HashMap::new(),
        contexts: HashMap::new(),
        flights: HashMap::new(),
        local_instance_id: deps.identity.instance_id.clone(),
        local_signing: deps.identity.signing_key.clone(),
        sender_epoch: deps.transport.boot_epoch().as_str().to_owned(),
        transport: deps.transport,
        executors: deps.executors,
        config: deps.config,
        hooks: deps.hooks,
        coordination: deps.coordination,
        cmd_rx,
        owner_event_rx,
        owner_event_tx: owner_event_tx.clone(),
        front_ctl_tx: deps.front_ctl_tx,
        cancel: deps.cancel,
        event_log: deps.event_log,
        event_notify: deps.event_notify,
        outbound_counter: 0,
        outbound_nonces: VecDeque::with_capacity(64),
        handle: driver_handle,
        idle_rx: deps.idle_rx,
        pending_idle_queries: HashMap::new(),
    };
    tokio::spawn(forward_verified_inbound(
        inbound,
        inbound_tx,
        inbound_cancel,
    ));
    tokio::spawn(driver.run());
    // owner_event_tx is cloned into spawned hook/write/wake
    // tasks. The original `owner_event_tx` clone is dropped
    // here so the channel closes when no task is running.
    drop(owner_event_tx);
    handle
}

struct Driver {
    /// Pure state machines, one per display.
    engines: HashMap<DisplayId, ClaimEngine>,
    /// Generation-stable display facts.
    contexts: HashMap<DisplayId, DisplayCtx>,
    /// Per-display live flight tracking (nonce + peer instance id).
    flights: HashMap<DisplayId, ActiveFlight>,
    /// Local instance id.
    local_instance_id: String,
    /// Local signing key (for outbound frame signing).
    local_signing: ed25519_dalek::SigningKey,
    /// Sender epoch (mirrors the transport's boot epoch in this arc).
    sender_epoch: String,
    /// Authenticated transport.
    transport: Arc<ClaimTransportHandle>,
    executors: watch::Receiver<Arc<HashMap<DisplayId, Arc<dyn CommandSink>>>>,
    config: watch::Receiver<Arc<Config>>,
    hooks: Arc<HookEngine>,
    coordination: Option<CoordinationHandle>,
    cmd_rx: mpsc::Receiver<RuntimeEvent>,
    /// Inbound channel for asynchronous owner-event completions
    /// from hook slots and write/wake tasks. Cloned into each
    /// spawned task so the spawned task's `send` is the only
    /// sender left when the dispatch loop is in a quiescent
    /// state.
    owner_event_rx: mpsc::Receiver<(DisplayId, String, OwnerEvent)>,
    /// Sender half of the owner-event channel. Cloned into
    /// each spawned task; the original is dropped after spawn
    /// so the channel closes cleanly when no work is pending.
    owner_event_tx: mpsc::Sender<(DisplayId, String, OwnerEvent)>,
    /// Daemon-front control channel (forwarded to the engine).
    /// Used to publish `ControlMsg::SetClaimSuppression` so the
    /// rules engine can gate ownership-loss reactions (F10).
    front_ctl_tx: mpsc::Sender<dormant_core::rules::ControlMsg>,
    cancel: CancellationToken,
    event_log: Option<Arc<Mutex<Vec<String>>>>,
    event_notify: Option<Arc<Notify>>,
    outbound_counter: u64,
    outbound_nonces: VecDeque<String>,
    /// Back-reference to the handle so the driver can update the
    /// F10 suppression side-table synchronously.
    handle: ClaimRuntimeHandle,
    /// Daemon-lifetime idle observation channel — read by the
    /// owner-side `IdleQuery` handler to answer remote idle queries
    /// with the local idle duration.
    #[allow(dead_code, reason = "consumed by IdleQuery handler")]
    idle_rx: Option<crate::idle_observation::IdleObservationRx>,
    /// Pending idle queries keyed by nonce.  Each entry carries the
    /// queried display so the `IdleReport` handler can verify the
    /// sender is the expected owner.
    pending_idle_queries: HashMap<String, (DisplayId, tokio::sync::oneshot::Sender<u64>)>,
}

impl Driver {
    /// VCP 0x60 reports `0x00` for a panel in standby (the
    /// DDC/CI standard's reserved "no active input" code).
    /// F4 forbids the direct fallback when the read lands on
    /// this value — the panel is powered off, the read is
    /// honest, and the operator must wake it first.
    const MAGIC_STANDBY: u8 = 0x00;

    async fn run(mut self) {
        self.refresh_contexts_from_config().await;
        let mut tick = tokio::time::interval(DEADLINE_SWEEP_PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => break,
                maybe = self.cmd_rx.recv() => {
                    let Some(event) = maybe else { break };
                    self.handle_event(event).await;
                }
                maybe = self.owner_event_rx.recv() => {
                    let Some((display, nonce, event)) = maybe else { break };
                    self.handle_owner_completion(&display, &nonce, event);
                }
                _ = tick.tick() => {
                    self.sweep_deadlines();
                    self.auto_disarm_expired();
                }
                changed = self.config.changed() => {
                    if changed.is_err() { break; }
                    self.refresh_contexts_from_config().await;
                }
                changed = self.executors.changed() => {
                    if changed.is_err() { break; }
                    self.refresh_contexts_from_config().await;
                }
            }
        }
    }

    async fn handle_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Inbound(frame) => {
                self.handle_inbound(frame);
            }
            RuntimeEvent::ClaimShared { display, reply } => {
                self.handle_claim_shared(&display, reply).await;
            }
            RuntimeEvent::ClaimArm { display, reply } => {
                self.handle_claim_arm(&display, reply);
            }
            RuntimeEvent::DisplayRemoved(display) => {
                self.handle_display_removed(&display);
            }
            RuntimeEvent::IdleQuery { display, reply } => {
                self.handle_idle_query(display, reply);
            }
            #[cfg(any(test, feature = "test-util"))]
            RuntimeEvent::InjectOwnerCompletion {
                display,
                nonce,
                event,
                reply,
            } => {
                let accepted = self.handle_owner_completion(&display, &nonce, event);
                let _ = reply.send(accepted);
            }
            #[cfg(any(test, feature = "test-util"))]
            RuntimeEvent::InjectIdleReport { nonce, idle_ms } => {
                if let Some((_display, tx)) = self.pending_idle_queries.remove(&nonce) {
                    let _ = tx.send(idle_ms);
                }
            }
            #[cfg(any(test, feature = "test-util"))]
            RuntimeEvent::RequesterNonce { display, reply } => {
                let nonce = self
                    .flights
                    .get(&display)
                    .map(|flight| flight.nonce.clone());
                let _ = reply.send(nonce);
            }
        }
    }

    fn handle_owner_completion(
        &mut self,
        display: &DisplayId,
        nonce: &str,
        event: OwnerEvent,
    ) -> bool {
        // F2: hook/write/wake completions from a terminalised flight must not
        // advance a newer flight for the same display or lift its suppression.
        let current_nonce = self
            .flights
            .get(display)
            .map(|flight| flight.nonce.as_str());
        if current_nonce != Some(nonce) {
            return false;
        }
        // Route requester-side `before_acquire` completions to the
        // requester engine — the `owner_event` path silently drops
        // completions for requester flights (the engine match is on
        // `Flight::Owner`).
        let is_requester = self
            .engines
            .get(display)
            .and_then(|e| e.requester_stage(display))
            .is_some();
        if is_requester {
            let requester_event = match event {
                OwnerEvent::BeforeRelease(HookResult::Completed) => {
                    RequesterEvent::AcquireCompleted
                }
                OwnerEvent::BeforeRelease(HookResult::Aborted) => RequesterEvent::AcquireFailed {
                    reason: "hook aborted".to_owned(),
                },
                _ => return false,
            };
            let actions = self.feed_requester_event(display, requester_event);
            self.dispatch_actions(display, &actions);
            return true;
        }
        let actions = self.feed_owner_event(display, event);
        self.dispatch_actions(display, &actions);
        true
    }

    #[allow(clippy::too_many_lines)]
    fn handle_inbound(&mut self, frame: ClaimFrame) {
        let ClaimFrame {
            sender_instance_id,
            sender_epoch,
            nonce,
            message,
            ..
        } = frame;
        match message {
            ClaimMessage::ClaimRequest(request) => {
                self.handle_inbound_request(sender_instance_id, sender_epoch, nonce, request);
            }
            ClaimMessage::ClaimAbort(_abort) => {
                // Nonce correlate against active owner flights.
                let Some(display) = self.find_display_by_owner_nonce(&nonce) else {
                    return;
                };
                let actions = self
                    .engines
                    .get_mut(&display)
                    .expect("display has engine")
                    .owner_event(&display, OwnerEvent::abort(nonce));
                self.dispatch_actions(&display, &actions);
            }
            ClaimMessage::ClaimAcquireReady(ready) => {
                // Nonce-correlate against active owner flights.
                // The owner must reject an AcquireReady whose nonce
                // doesn't match the active flight.
                let Some(display) = self.find_display_by_owner_nonce(&ready.request_nonce) else {
                    return;
                };
                let actions = self
                    .engines
                    .get_mut(&display)
                    .expect("display has engine")
                    .owner_event(&display, OwnerEvent::AcquireReady);
                self.dispatch_actions(&display, &actions);
            }
            ClaimMessage::ReleaseFailed(release) => {
                let Some(display) = self.find_display_by_requester_nonce(&release.nonce) else {
                    return;
                };
                let actions = self
                    .engines
                    .get_mut(&display)
                    .expect("display has engine")
                    .requester_event(
                        &display,
                        RequesterEvent::release_failed(release.nonce, release.reason),
                        Instant::now(),
                    );
                self.dispatch_actions(&display, &actions);
                if self
                    .engines
                    .get(&display)
                    .and_then(|e| e.requester_stage(&display))
                    .is_none()
                {
                    self.flights.remove(&display);
                    self.clear_claim_suppression(&display);
                }
            }
            ClaimMessage::ClaimResponse(response) => {
                let Some(display) = self.find_display_by_requester_nonce(&response.nonce) else {
                    return;
                };
                let accepted = matches!(&response.verdict, ClaimVerdict::Accepted { .. });
                if accepted
                    || self
                        .flights
                        .get(&display)
                        .is_some_and(|flight| flight.peer_instance_id.is_empty())
                {
                    if let Some(flight) = self.flights.get_mut(&display) {
                        flight.peer_instance_id.clone_from(&sender_instance_id);
                        flight.peer_epoch.clone_from(&sender_epoch);
                    }
                }
                let actions = self
                    .engines
                    .get_mut(&display)
                    .expect("display has engine")
                    .requester_event(
                        &display,
                        RequesterEvent::Response {
                            nonce: response.nonce,
                            peer_instance_id: sender_instance_id.clone(),
                            verdict: response.verdict,
                        },
                        Instant::now(),
                    );
                if accepted
                    && actions
                        .iter()
                        .any(|action| matches!(action, Action::Trace("claim_accepted")))
                {
                    if let Some(coordination) = &self.coordination {
                        coordination.set_owner(&display, Some(sender_instance_id));
                    }
                }
                self.dispatch_actions(&display, &actions);
                if self
                    .engines
                    .get(&display)
                    .and_then(|e| e.requester_stage(&display))
                    .is_none()
                {
                    self.flights.remove(&display);
                    self.clear_claim_suppression(&display);
                }
            }
            ClaimMessage::IdleQuery(query) => {
                // Owner side: only reply if we own the queried display.
                // Non-owners drop silently — the spec defines IdleQuery
                // as requester→owner, not a cross-pair idle probe.
                let owned = self
                    .display_by_identity(&query.display_identity)
                    .is_some_and(|d| {
                        self.coordination
                            .as_ref()
                            .is_some_and(|c| c.snapshot().get(&d).is_some_and(|r| r.owned))
                    });

                if !owned {
                    return;
                }

                let idle_ms = self
                    .idle_rx
                    .as_ref()
                    .and_then(|rx| {
                        let obs = rx.borrow();
                        crate::idle_observation::idle_ms(&obs, Instant::now())
                    })
                    .unwrap_or(0);
                let report_frame = match sign_frame_for_peer(
                    &InstanceIdentity {
                        instance_id: self.local_instance_id.clone(),
                        signing_key: self.local_signing.clone(),
                        verifying_key: self.local_signing.verifying_key(),
                    },
                    self.sender_epoch.clone(),
                    sender_instance_id.clone(),
                    sender_epoch.clone(),
                    self.next_counter(),
                    self.next_nonce(),
                    ClaimMessage::IdleReport(dormant_core::claim::IdleReport {
                        idle_ms,
                        counter: self.outbound_counter,
                        nonce: nonce.clone(),
                    }),
                ) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(event = "idle_report_sign_failed", error = %e);
                        return;
                    }
                };
                let transport = self.transport.clone();
                let peer_id = sender_instance_id.clone();
                tokio::spawn(async move {
                    transport.send_response(&peer_id, &report_frame).await;
                });
                self.record_event("idle_report_sent");
            }
            ClaimMessage::IdleReport(report) => {
                let Some(display) = self
                    .pending_idle_queries
                    .get(&report.nonce)
                    .map(|(display, _)| display.clone())
                else {
                    return;
                };
                let sender_is_expected_owner = self
                    .coordination
                    .as_ref()
                    .and_then(|c| c.snapshot().get(&display).cloned())
                    .and_then(|r| r.owner_instance_id)
                    .is_some_and(|owner| owner == sender_instance_id);
                if !sender_is_expected_owner {
                    return;
                }
                if let Some((_display, tx)) = self.pending_idle_queries.remove(&report.nonce) {
                    let _ = tx.send(report.idle_ms);
                }
            }
        }
    }

    fn handle_inbound_request(
        &mut self,
        sender_instance_id: String,
        sender_epoch: String,
        nonce: String,
        request: ClaimRequest,
    ) {
        let Some(display) = self.find_display_by_claim_identity(&request.display_identity) else {
            // Unknown display: NotOwner verdict back.
            self.send_verdict_to_peer_now(
                &sender_instance_id,
                &sender_epoch,
                &nonce,
                ClaimVerdict::NotOwner,
            );
            return;
        };
        let Some(ctx) = self.contexts.get(&display).cloned() else {
            self.send_verdict_to_peer_now(
                &sender_instance_id,
                &sender_epoch,
                &nonce,
                ClaimVerdict::NotOwner,
            );
            return;
        };
        let capability = if ctx.writable {
            ClaimCapability::Writable
        } else {
            ClaimCapability::ObserveOnly
        };
        let disposition = if let Some(coord) = &self.coordination {
            let records = coord.snapshot();
            match records.get(&display) {
                Some(record) if record.owned => OwnerDisposition::Ready {
                    standby: record
                        .panel_state
                        .as_ref()
                        .and_then(|p| match p.power {
                            Some(dormant_core::traits::PowerState::Standby) => Some(true),
                            _ => Some(false),
                        })
                        .unwrap_or(false),
                },
                Some(_) => OwnerDisposition::NotOwner,
                None => OwnerDisposition::Quiescing,
            }
        } else {
            OwnerDisposition::NotOwner
        };
        let eta = match disposition {
            OwnerDisposition::Ready { standby: true } => self.release_deadline_cap(),
            _ => sum_blocking_before_release(&ctx.hooks),
        };
        let owner_request = OwnerRequest {
            display: display.clone(),
            requested_identity: request.display_identity.clone(),
            local_identity: ctx.claim_identity.clone(),
            requester_input_code: u16::from(request.requester_input_code),
            local_input_code: u16::from(ctx.local_input_code.unwrap_or(0)),
            capability,
            disposition,
            eta,
        };
        let release_cap = self.release_deadline_cap();
        let now = Instant::now();
        let engine = self.engines.entry(display.clone()).or_insert_with(|| {
            ClaimEngine::with_release_policy(Duration::from_secs(2), release_cap)
        });
        let actions = engine.begin_owner(owner_request, &nonce, now, release_cap);
        // Record the flight: the nonce is now the owner-side
        // correlation key; the peer is the inbound sender.
        // The OWNER writes the REQUESTER's input code (to
        // hand the panel over), not the local code — track
        // the code-to-write explicitly so the driver can pick
        // it without re-deriving it from the engine's
        // unparameterized `WriteInput` action.
        self.flights.insert(
            display.clone(),
            ActiveFlight {
                nonce: nonce.clone(),
                peer_instance_id: sender_instance_id,
                peer_epoch: sender_epoch,
                write_code: u8::try_from(request.requester_input_code).unwrap_or(0),
            },
        );
        self.dispatch_actions(&display, &actions);
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_claim_shared(
        &mut self,
        display: &DisplayId,
        reply: oneshot::Sender<ClaimSharedResult>,
    ) {
        let Some(ctx) = self.contexts.get(display).cloned() else {
            let did = display.0.as_str();
            info!(event = "claim_denied", reason = "no_context", display_id = %did);
            let _ = reply.send(ClaimSharedResult::Denied(ClaimDeniedReason::Unsupported));
            return;
        };
        if !ctx.writable {
            let did = display.0.as_str();
            info!(event = "claim_denied", reason = "not_writable", display_id = %did);
            let _ = reply.send(ClaimSharedResult::Denied(ClaimDeniedReason::Unsupported));
            return;
        }
        if ctx.claim_identity.is_none() {
            let _ = reply.send(ClaimSharedResult::Denied(
                ClaimDeniedReason::IdentityUnavailable,
            ));
            return;
        }
        let busy = self.engines.get(display).is_some_and(|engine| {
            engine.requester_stage(display).is_some() || engine.owner_stage(display).is_some()
        });
        if busy {
            self.record_event("claim_busy");
            let _ = reply.send(ClaimSharedResult::Busy);
            return;
        }
        if self.transport.addressable_peer_count() == 0 {
            self.record_no_addressable_peers(display, "preflight", FanoutResult::default());
            let _ = reply.send(ClaimSharedResult::Denied(
                ClaimDeniedReason::CoordinationDisabled,
            ));
            return;
        }

        let nonce = self.next_nonce();
        self.next_counter();
        let Some((counter, frame_nonce, message)) = self.build_claim_request(display, &nonce)
        else {
            let did = display.0.as_str();
            info!(event = "claim_denied", reason = "request_build_failed", display_id = %did);
            let _ = reply.send(ClaimSharedResult::Denied(ClaimDeniedReason::Unsupported));
            return;
        };
        // Per-peer dials are concurrent and bounded; awaiting them keeps the
        // successful frame-write count authoritative before negotiation is promised.
        let fanout = self
            .transport
            .fanout_request(counter, &frame_nonce, &message)
            .await;
        let Some(peer_count) = negotiated_peer_count(fanout) else {
            self.record_no_addressable_peers(display, "fanout", fanout);
            let _ = reply.send(ClaimSharedResult::Denied(
                ClaimDeniedReason::CoordinationDisabled,
            ));
            return;
        };

        let now = Instant::now();
        let claim_timeout = self.claim_timeout();
        let release_cap = self.release_deadline_cap();
        let engine = self.engines.entry(display.clone()).or_insert_with(|| {
            ClaimEngine::with_release_policy(Duration::from_secs(2), release_cap)
        });
        let actions =
            engine.begin_requester(display.clone(), &nonce, peer_count, now, claim_timeout);
        for action in &actions {
            self.record_action(action);
        }
        if actions
            .iter()
            .any(|action| matches!(action, Action::BusyLocal))
        {
            let _ = reply.send(ClaimSharedResult::Busy);
            return;
        }

        let front_ctl_tx = self.front_ctl_tx.clone();
        let local_input_code = ctx.effective_write_code().unwrap_or(0);
        let suppressed_until = now + claim_timeout;
        if let Ok(mut map) = self.handle.suppressed.lock() {
            map.insert(display.clone(), suppressed_until);
        }
        let display_for_suppression = display.clone();
        tokio::spawn(async move {
            let _ = front_ctl_tx
                .send(dormant_core::rules::ControlMsg::SetClaimSuppression {
                    display: display_for_suppression,
                    until: Some(suppressed_until),
                })
                .await;
        });
        self.flights.insert(
            display.clone(),
            ActiveFlight {
                nonce: nonce.clone(),
                peer_instance_id: String::new(),
                peer_epoch: String::new(),
                write_code: local_input_code,
            },
        );
        let _ = self
            .engines
            .get_mut(display)
            .expect("engine present")
            .requester_event(display, RequesterEvent::FanoutSent, now);
        let _ = reply.send(ClaimSharedResult::Accepted {
            deadline: suppressed_until,
        });
    }

    fn handle_claim_arm(
        &mut self,
        display: &DisplayId,
        reply: oneshot::Sender<Result<Instant, ArmFailure>>,
    ) {
        let Some(ctx) = self.contexts.get(display) else {
            let _ = reply.send(Err(ArmFailure::NotClaimCapable));
            return;
        };
        if !ctx.writable || ctx.claim_identity.is_none() {
            let _ = reply.send(Err(ArmFailure::NotClaimCapable));
            return;
        }
        let armed_window = self.armed_window();
        let deadline = Instant::now() + armed_window;
        if let Ok(mut armed) = self.handle.armed.lock() {
            armed.insert(display.clone(), deadline);
        }
        let _ = reply.send(Ok(deadline));
    }

    fn handle_display_removed(&mut self, display: &DisplayId) {
        if let Some(engine) = self.engines.get_mut(display) {
            let stage = engine.requester_stage(display);
            let actions = if stage.is_some() {
                engine.requester_event(display, RequesterEvent::DisplayRemoved, Instant::now())
            } else {
                engine.owner_event(display, OwnerEvent::DisplayRemoved)
            };
            self.dispatch_actions(display, &actions);
        }
        self.contexts.remove(display);
        self.flights.remove(display);
        if let Ok(mut map) = self.handle.suppressed.lock() {
            map.remove(display);
        }
    }

    /// Find a display by its EDID-derived claim identity.
    fn display_by_identity(&self, identity: &str) -> Option<DisplayId> {
        self.contexts.iter().find_map(|(display, ctx)| {
            ctx.claim_identity
                .as_ref()
                .and_then(|ci| (ci == identity).then_some(display.clone()))
        })
    }

    fn handle_idle_query(&mut self, display: DisplayId, reply: oneshot::Sender<Option<u64>>) {
        let display_identity = self
            .contexts
            .get(&display)
            .and_then(|ctx| ctx.claim_identity.clone())
            .unwrap_or_default();
        let nonce = self.next_nonce();
        let counter = self.next_counter();
        let message = ClaimMessage::IdleQuery(dormant_core::claim::IdleQuery {
            display_identity,
            nonce: nonce.clone(),
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        // SEC-4: prune stale entries before inserting — a timed-out
        // query leaves a dead sender in the map.  Sweep before every insert
        // to keep the map bounded.
        self.pending_idle_queries
            .retain(|_k, (_display, tx)| !tx.is_closed());
        self.pending_idle_queries
            .insert(nonce.clone(), (display.clone(), resp_tx));
        // Log the nonce so tests can observe it and inject a matching
        // IdleReport via the test seam.
        {
            append_event(
                self.event_log.as_ref(),
                self.event_notify.as_ref(),
                format!("idle_query_sent nonce={nonce}"),
            );
        }

        let timeout = self.claim_timeout();
        let transport = self.transport.clone();
        tokio::spawn(async move {
            transport.fanout_request(counter, &nonce, &message).await;
        });
        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, resp_rx).await;
            match result {
                Ok(Ok(ms)) => {
                    let _ = reply.send(Some(ms));
                }
                _ => {
                    let _ = reply.send(None);
                }
            }
        });
    }

    fn record_action(&self, action: &Action) {
        if let Action::Trace(name) = action {
            self.record_event(name);
        }
    }

    fn record_event(&self, name: &str) {
        append_event(
            self.event_log.as_ref(),
            self.event_notify.as_ref(),
            name.to_string(),
        );
        match name {
            "claim_requested" => info!(event = "claim_requested", "claim lifecycle"),
            "claim_accepted" => info!(event = "claim_accepted", "claim lifecycle"),
            "claim_denied" => info!(event = "claim_denied", "claim lifecycle"),
            "claim_busy" => info!(event = "claim_busy", "claim lifecycle"),
            "claim_not_owner" => info!(event = "claim_not_owner", "claim lifecycle"),
            "claim_release_aborted" => info!(event = "claim_release_aborted", "claim lifecycle"),
            "claim_release_failed" => info!(event = "claim_release_failed", "claim lifecycle"),
            "claim_fallback_direct" => info!(event = "claim_fallback_direct", "claim lifecycle"),
            "claim_failed" => info!(event = "claim_failed", "claim lifecycle"),
            "claim_completed" => info!(event = "claim_completed", "claim lifecycle"),
            "claim_acquire_failed" => info!(event = "claim_acquire_failed", "claim lifecycle"),
            "claim_acquire_ready" => info!(event = "claim_acquire_ready", "claim lifecycle"),
            "claim_acquire_wait_expired" => {
                info!(event = "claim_acquire_wait_expired", "claim lifecycle");
            }
            _ => {}
        }
    }

    fn record_no_addressable_peers(
        &self,
        display: &DisplayId,
        phase: &'static str,
        result: FanoutResult,
    ) {
        self.record_event("claim_no_addressable_peers");
        let display_id = display.0.as_str();
        info!(
            event = "claim_no_addressable_peers",
            display = display_id,
            phase,
            contacted = result.contacted,
            skipped_no_endpoint = result.skipped_no_endpoint,
            skipped_no_epoch = result.skipped_no_epoch,
            sign_failed = result.sign_failed,
            dial_failed = result.dial_failed,
        );
    }

    fn sweep_deadlines(&mut self) {
        let now = Instant::now();
        let displays: Vec<DisplayId> = self.engines.keys().cloned().collect();
        let mut lift_displays = Vec::new();
        let mut to_dispatch: Vec<(DisplayId, Vec<Action>, bool)> = Vec::new();
        for display in displays {
            let Some(engine) = self.engines.get_mut(&display) else {
                continue;
            };
            let actions = engine.on_deadline(&display, now);
            if !engine.is_suppressed(&display, now) {
                if let Ok(mut map) = self.handle.suppressed.lock() {
                    map.remove(&display);
                }
                lift_displays.push(display.clone());
            }
            if !actions.is_empty() {
                let terminal = actions
                    .iter()
                    .any(|action| matches!(action, Action::Terminal(_)));
                to_dispatch.push((display.clone(), actions, terminal));
            }
        }
        for display in lift_displays {
            let tx = self.front_ctl_tx.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(dormant_core::rules::ControlMsg::SetClaimSuppression {
                        display,
                        until: None,
                    })
                    .await;
            });
        }
        for (display, actions, terminal) in to_dispatch {
            self.dispatch_actions(&display, &actions);
            // Terminal sends still need the peer route stored in ActiveFlight.
            // Cleanup only after dispatch has cloned that route into outbound tasks.
            if terminal {
                self.flights.remove(&display);
            }
        }
    }

    fn auto_disarm_expired(&mut self) {
        let now = Instant::now();
        let expired: Vec<DisplayId> = self
            .handle
            .armed
            .lock()
            .map(|g| {
                g.iter()
                    .filter_map(|(d, deadline)| (now >= *deadline).then_some(d.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if !expired.is_empty() {
            if let Ok(mut g) = self.handle.armed.lock() {
                for d in expired {
                    g.remove(&d);
                }
            }
        }
    }

    fn dispatch_actions(&mut self, display: &DisplayId, actions: &[Action]) {
        // Iterative work-queue drain. The engine can chain
        // (e.g. owner `RunBeforeRelease` -> `BeforeRelease`
        // event -> `WriteInput` -> `AfterRelease` event -> ...),
        // so we seed a queue and re-feed it from the engine
        // until empty. The queue is bounded by the engine's own
        // single-flight invariant, so this terminates in O(1)
        // cycles for a legal sequence.
        let mut queue: VecDeque<Action> = actions.iter().cloned().collect();
        while let Some(action) = queue.pop_front() {
            self.record_action(&action);
            let produced = self.execute_action(display, action);
            for action in produced {
                queue.push_back(action);
            }
        }
    }

    /// Clear the F10 suppression for `display` — both the local
    /// handle side-table and the rules engine's copy via
    /// `ControlMsg::SetClaimSuppression`.
    fn clear_claim_suppression(&self, display: &DisplayId) {
        if let Ok(mut map) = self.handle.suppressed.lock() {
            map.remove(display);
        }
        let front_ctl_tx = self.front_ctl_tx.clone();
        let display_for_task = display.clone();
        tokio::spawn(async move {
            let _ = front_ctl_tx
                .send(dormant_core::rules::ControlMsg::SetClaimSuppression {
                    display: display_for_task,
                    until: None,
                })
                .await;
        });
    }

    fn execute_action(&mut self, display: &DisplayId, action: Action) -> Vec<Action> {
        match action {
            Action::SendAbort => {
                let display = display.clone();
                let mut flight = self.flights.get(&display).cloned();

                let transport = self.transport.clone();
                let identity = self.local_identity_view();
                let sender_epoch = self.sender_epoch.clone();

                let counter = self.next_counter();
                tokio::spawn(async move {
                    let Some(flight) = flight.take() else { return };
                    let peer_instance_id = flight.peer_instance_id;
                    let peer_epoch = flight.peer_epoch;
                    if peer_instance_id.is_empty() || peer_epoch.is_empty() {
                        return;
                    }
                    if let Ok(frame) = sign_frame_for_peer(
                        &identity,
                        sender_epoch,
                        peer_instance_id.clone(),
                        peer_epoch,
                        counter,
                        format!("abort-{}", flight.nonce),
                        ClaimMessage::ClaimAbort(ClaimAbort {
                            nonce: flight.nonce,
                        }),
                    ) {
                        transport.send_abort(&peer_instance_id, &frame).await;
                    }
                });
                Vec::new()
            }
            Action::AttemptFallback => {
                self.attempt_fallback(display);
                Vec::new()
            }
            Action::WatchForFlip => {
                // Watching is engine state only. Suppression was registered when
                // the request began, so this action dispatches no additional I/O.
                Vec::new()
            }
            Action::BroadcastRequest | Action::RetryRequest | Action::RetryWithEpoch(_) => {
                // Re-fan-out a request frame (retry uses the
                // same nonce; epoch retry re-uses the
                // requester instance + code with a refreshed
                // epoch).
                if let Some((counter, frame_nonce, message)) =
                    self.build_claim_request(display, &self.nonce_of(display))
                {
                    let transport = self.transport.clone();
                    tokio::spawn(async move {
                        transport
                            .fanout_request(counter, &frame_nonce, &message)
                            .await;
                    });
                }
                Vec::new()
            }
            Action::RunBeforeRelease => {
                self.run_hook_slot(display, Direction::Release, Phase::Before, false)
            }
            Action::SendAcquireReady => {
                self.send_acquire_ready_to_owner(display);
                Vec::new()
            }
            Action::WriteInput => self.write_input_source(display),
            Action::RunAfterRelease { aborted } => {
                self.run_hook_slot(display, Direction::Release, Phase::After, aborted)
            }
            Action::RunBeforeAcquire => {
                self.run_hook_slot(display, Direction::Acquire, Phase::Before, false)
            }
            Action::WakeDisplay => self.wake_display(display),
            Action::SendVerdict(verdict) => {
                self.send_owner_verdict(display, verdict);
                // After sending the verdict, drive the OWNER
                // path forward: the engine is in `OwnerStage::AckSent`
                // and must be fed `OwnerEvent::AckDelivered` to
                // progress to `RunBeforeRelease` → `WriteInput` →
                // `RunAfterRelease` → `Terminal::Released`. The
                // engine returns the next action sequence
                // (which is `RunBeforeRelease` for a powered
                // owner); we re-queue that.
                self.feed_owner_event(display, OwnerEvent::AckDelivered)
            }
            Action::SendReleaseFailed => {
                self.send_release_failed_to_requester(display);
                Vec::new()
            }
            Action::EnterDeferred | Action::Trace(_) | Action::Terminal(_) | Action::BusyLocal => {
                Vec::new()
            }
        }
    }

    fn run_hook_slot(
        &mut self,
        display: &DisplayId,
        direction: Direction,
        phase: Phase,
        aborted: bool,
    ) -> Vec<Action> {
        let Some(ctx) = self.contexts.get(display).cloned() else {
            return self.feed_owner_event_after_slot(display, direction, phase);
        };
        let hook_actions = ctx.hooks.slot_for(direction, phase).to_vec();
        if hook_actions.is_empty() {
            return self.feed_owner_event_after_slot(display, direction, phase);
        }
        let display_name = display.0.clone();
        let display_identity = ctx.claim_identity.clone().unwrap_or_default();
        let peer_instance_id = self
            .flights
            .get(display)
            .map(|f| f.peer_instance_id.clone())
            .unwrap_or_default();
        // The actual hook execution happens in a spawned task;
        // the result feeds back through `owner_event_tx` as
        // a follow-up `OwnerHookOutcome` runtime event. The
        // dispatch loop re-enters the engine from there.
        let hooks_engine = self.hooks.clone();
        let display_for_task = display.clone();
        let outcome_tx = self.owner_event_tx.clone();
        // F2: capture the flight nonce at arm time so the
        // spawned task can tag its completion. The driver
        // drops completions whose nonce doesn't match the
        // active flight (stale from a terminalised or
        // mid-claim-reloaded flight).
        let flight_nonce = self
            .flights
            .get(display)
            .map(|f| f.nonce.clone())
            .unwrap_or_default();
        tokio::spawn(async move {
            let context = HookContext {
                display: &display_name,
                display_identity: &display_identity,
                direction,
                phase,
                peer: &peer_instance_id,
                fallback: false,
                aborted,
            };
            let slot = HookSlot {
                context,
                actions: &hook_actions,
            };
            let outcome = hooks_engine.run_slot(slot).await;
            let event = match (phase, outcome) {
                (Phase::After, _) => OwnerEvent::AfterReleaseCompleted,
                (Phase::Before, HookOutcome::Completed { .. }) => {
                    OwnerEvent::BeforeRelease(HookResult::Completed)
                }
                (Phase::Before, HookOutcome::Aborted { reason, .. }) => {
                    let display_id = display_for_task.0.clone();
                    warn!(
                        event = "claim_release_aborted",
                        kind = "hook_aborted",
                        display_id = %display_id,
                        reason = %reason,
                    );
                    OwnerEvent::BeforeRelease(HookResult::Aborted)
                }
            };
            let _ = outcome_tx
                .send((display_for_task, flight_nonce, event))
                .await;
        });
        // No immediate follow-up actions; the spawned task
        // re-enters the engine when it completes.
        Vec::new()
    }

    fn feed_owner_event_after_slot(
        &mut self,
        display: &DisplayId,
        direction: Direction,
        phase: Phase,
    ) -> Vec<Action> {
        let is_requester = direction == Direction::Acquire
            && self
                .engines
                .get(display)
                .and_then(|e| e.requester_stage(display))
                .is_some();
        if is_requester {
            self.feed_requester_event(display, RequesterEvent::AcquireCompleted)
        } else {
            match phase {
                Phase::Before => {
                    self.feed_owner_event(display, OwnerEvent::BeforeRelease(HookResult::Completed))
                }
                Phase::After => self.feed_owner_event(display, OwnerEvent::AfterReleaseCompleted),
            }
        }
    }

    fn write_input_source(&mut self, display: &DisplayId) -> Vec<Action> {
        // The OWNER path uses the requester's input code (the
        // code the OWNER must select when handing the panel
        // over). The requester path uses the local code (the
        // fallback writes the local code). The flight record
        // carries the `write_code` set when the flight was
        // armed.
        let write_code = self.flights.get(display).map(|f| f.write_code).or_else(|| {
            self.contexts
                .get(display)
                .and_then(DisplayCtx::effective_write_code)
        });
        let Some((sink, local_code)) = self.lookup_executor(display) else {
            return self.feed_owner_event(
                display,
                OwnerEvent::WriteFailed("E_DISPLAY_IO: no executor".to_owned()),
            );
        };
        let target = write_code.unwrap_or(local_code);
        let display_for_task = display.clone();
        // F2: capture the flight nonce so the spawned task
        // can tag its completion.
        let flight_nonce = self
            .flights
            .get(display)
            .map(|f| f.nonce.clone())
            .unwrap_or_default();
        let outcome_tx = self.owner_event_tx.clone();
        tokio::spawn(async move {
            let result = sink
                .write_input_source(InputSourceTarget {
                    write_code: target,
                    expected_readback: InputSourceReadback::Exact(target),
                })
                .await;
            let event = match result {
                Ok(()) => OwnerEvent::WriteSucceeded,
                Err(failure) => OwnerEvent::WriteFailed(failure.error),
            };
            let _ = outcome_tx
                .send((display_for_task, flight_nonce, event))
                .await;
        });
        Vec::new()
    }

    fn wake_display(&mut self, display: &DisplayId) -> Vec<Action> {
        let Some(sink) = self.executors.borrow().get(display).cloned() else {
            return self.feed_owner_event(display, OwnerEvent::WakeCompleted);
        };
        let display_for_task = display.clone();
        // F2: capture the flight nonce so the spawned task
        // can tag its completion.
        let flight_nonce = self
            .flights
            .get(display)
            .map(|f| f.nonce.clone())
            .unwrap_or_default();
        let outcome_tx = self.owner_event_tx.clone();
        tokio::spawn(async move {
            let result = sink.wake().await;
            let event = match result {
                Ok(()) => OwnerEvent::WakeCompleted,
                Err(failure) => {
                    let display_id = display_for_task.0.clone();
                    warn!(
                        event = "claim_release_failed",
                        display_id = %display_id,
                        detail = %failure.error,
                    );
                    OwnerEvent::WriteFailed(failure.error)
                }
            };
            let _ = outcome_tx
                .send((display_for_task, flight_nonce, event))
                .await;
        });
        Vec::new()
    }

    fn feed_owner_event(&mut self, display: &DisplayId, event: OwnerEvent) -> Vec<Action> {
        let actions = self
            .engines
            .get_mut(display)
            .expect("engine present for owner event")
            .owner_event(display, event);
        if self
            .engines
            .get(display)
            .and_then(|e| e.owner_stage(display))
            .is_none()
        {
            self.flights.remove(display);
            // Owner flight terminal: lift F10 suppression.
            self.clear_claim_suppression(display);
        }
        actions
    }

    /// Feed a requester event to the engine and clean up when the flight terminates.
    fn feed_requester_event(&mut self, display: &DisplayId, event: RequesterEvent) -> Vec<Action> {
        let actions = self
            .engines
            .get_mut(display)
            .expect("engine present for requester event")
            .requester_event(display, event, Instant::now());
        if self
            .engines
            .get(display)
            .and_then(|e| e.requester_stage(display))
            .is_none()
        {
            self.flights.remove(display);
            self.clear_claim_suppression(display);
        }
        actions
    }

    fn lookup_executor(&self, display: &DisplayId) -> Option<(Arc<dyn CommandSink>, u8)> {
        let map = self.executors.borrow();
        let sink = map.get(display).cloned()?;
        let code = self
            .contexts
            .get(display)
            .and_then(DisplayCtx::effective_write_code)?;
        Some((sink, code))
    }

    fn send_owner_verdict(&mut self, display: &DisplayId, verdict: ClaimVerdict) {
        let Some(flight) = self.flights.get(display).cloned() else {
            return;
        };
        if flight.peer_instance_id.is_empty() || flight.peer_epoch.is_empty() {
            return;
        }
        self.send_verdict_to_peer_now(
            &flight.peer_instance_id,
            &flight.peer_epoch,
            &flight.nonce,
            verdict,
        );
    }

    fn send_verdict_to_peer_now(
        &mut self,
        peer: &str,
        peer_epoch: &str,
        nonce: &str,
        verdict: ClaimVerdict,
    ) {
        let counter = self.next_counter();
        let transport = self.transport.clone();
        let identity = self.local_identity_view();
        let sender_epoch = self.sender_epoch.clone();
        let peer = peer.to_owned();
        let peer_epoch = peer_epoch.to_owned();
        let nonce = nonce.to_owned();
        tokio::spawn(async move {
            let Ok(frame) = sign_frame_for_peer(
                &identity,
                sender_epoch,
                peer.clone(),
                peer_epoch,
                counter,
                format!("resp-{nonce}"),
                ClaimMessage::ClaimResponse(ClaimResponse {
                    nonce: nonce.clone(),
                    verdict,
                }),
            ) else {
                return;
            };
            transport.send_response(&peer, &frame).await;
        });
    }

    fn send_acquire_ready_to_owner(&mut self, display: &DisplayId) {
        let Some(flight) = self.flights.get(display).cloned() else {
            return;
        };
        if flight.peer_instance_id.is_empty() || flight.peer_epoch.is_empty() {
            return;
        }
        let counter = self.next_counter();
        let transport = self.transport.clone();
        let identity = self.local_identity_view();
        let sender_epoch = self.sender_epoch.clone();
        let peer = flight.peer_instance_id;
        let peer_epoch = flight.peer_epoch;
        let nonce = flight.nonce;
        tokio::spawn(async move {
            let Ok(frame) = sign_frame_for_peer(
                &identity,
                sender_epoch,
                peer.clone(),
                peer_epoch,
                counter,
                format!("acq-{nonce}"),
                ClaimMessage::ClaimAcquireReady(dormant_core::claim::ClaimAcquireReady {
                    nonce: format!("acq-{nonce}"),
                    request_nonce: nonce.clone(),
                }),
            ) else {
                return;
            };
            transport.send_acquire_ready(&peer, &frame).await;
        });
    }

    fn send_release_failed_to_requester(&mut self, display: &DisplayId) {
        let Some(flight) = self.flights.get(display).cloned() else {
            return;
        };
        if flight.peer_instance_id.is_empty() || flight.peer_epoch.is_empty() {
            return;
        }
        let counter = self.next_counter();
        let transport = self.transport.clone();
        let identity = self.local_identity_view();
        let sender_epoch = self.sender_epoch.clone();
        let peer = flight.peer_instance_id;
        let peer_epoch = flight.peer_epoch;
        let nonce = flight.nonce;
        tokio::spawn(async move {
            let Ok(frame) = sign_frame_for_peer(
                &identity,
                sender_epoch,
                peer.clone(),
                peer_epoch,
                counter,
                format!("relfail-{nonce}"),
                ClaimMessage::ReleaseFailed(ReleaseFailed {
                    nonce: nonce.clone(),
                    reason: "write failed".to_owned(),
                }),
            ) else {
                return;
            };
            transport.send_release_failed(&peer, &frame).await;
        });
    }

    fn attempt_fallback(&mut self, display: &DisplayId) {
        // The pure engine's `on_deadline` already removed the
        // requester flight (the `RequesterUnaccepted` branch
        // returns the `[SendAbort, Trace, AttemptFallback]`
        // action list with NO terminal — the flight itself
        // is gone). The fallback work that follows is
        // fire-and-forget: the engine has nothing left to
        // advance, and the operator's verdict is the
        // `event_log` entry this spawned task appends.
        //
        // No `WriteFailed("acquired")` event is fed back to
        // the engine: the requester flight is gone, the
        // signal would be a no-op, and a stale completion
        // keyed only by `DisplayId` (Must-F2) would race
        // against any new flight that starts in the
        // meantime. The log entry is the only state the
        // driver surfaces.
        let Some(sink) = self.executors.borrow().get(display).cloned() else {
            self.feed_requester_failed(
                display,
                ClaimFailure::Denied(ClaimDeniedReason::Unsupported),
            );
            return;
        };
        let Some(target_code) = self
            .contexts
            .get(display)
            .and_then(DisplayCtx::effective_write_code)
        else {
            self.feed_requester_failed(
                display,
                ClaimFailure::Denied(ClaimDeniedReason::Unsupported),
            );
            return;
        };
        self.record_event("claim_fallback_direct");
        let event_log = self.event_log.clone();
        let event_notify = self.event_notify.clone();
        let display_label = display.0.clone();
        let task =
            Self::run_direct_fallback(sink, target_code, display_label, event_log, event_notify);
        tokio::spawn(task);
    }

    /// Terminal fallback outcome task — extracted so each outcome's
    /// `tracing::info!` anchor can be proved by a subscriber test.
    ///
    /// Called by [`Driver::attempt_fallback`] via `tokio::spawn`.
    pub(crate) async fn run_direct_fallback(
        sink: Arc<dyn CommandSink>,
        target_code: u8,
        display_label: String,
        event_log: Option<Arc<Mutex<Vec<String>>>>,
        event_notify: Option<Arc<Notify>>,
    ) {
        let Ok(Some(observed)) = sink.read_input_source_sampled().await else {
            append_event(
                event_log.as_ref(),
                event_notify.as_ref(),
                "claim_failed:identity_unavailable",
            );
            tracing::info!(
                event = "claim_failed",
                display_id = %display_label,
                reason = "identity_unavailable",
            );
            return;
        };
        if observed == target_code {
            // The hardware is already on the local code:
            // the direct fallback was a no-op. Surface
            // `claim_completed` so the operator's UI can
            // distinguish "the panel flipped" from "we
            // didn't need to flip it".
            append_event(event_log.as_ref(), event_notify.as_ref(), "claim_completed");
            tracing::info!(
                event = "claim_completed",
                display_id = %display_label,
                reason = "already_on_target",
            );
            return;
        }
        // VCP 0x60 reports 0x00 for a panel in standby
        // (the DDC/CI standard's reserved "no active
        // input" code). F4 forbids the direct fallback.
        if observed == Self::MAGIC_STANDBY {
            append_event(
                event_log.as_ref(),
                event_notify.as_ref(),
                "claim_failed:standby",
            );
            tracing::info!(
                event = "claim_failed",
                display_id = %display_label,
                reason = "standby",
            );
            return;
        }
        if let Err(_failure) = sink
            .write_input_source(InputSourceTarget {
                write_code: target_code,
                expected_readback: InputSourceReadback::Exact(target_code),
            })
            .await
        {
            append_event(
                event_log.as_ref(),
                event_notify.as_ref(),
                "claim_failed:write",
            );
            tracing::info!(
                event = "claim_failed",
                display_id = %display_label,
                reason = "write",
            );
        } else {
            // The direct write succeeded. The next
            // coordination poll will observe the flip
            // and feed `OwnershipChanged(true)` to the
            // rules engine (the fallback's contribution
            // ends here; the rules engine drives the
            // post-flip state machine).
            append_event(
                event_log.as_ref(),
                event_notify.as_ref(),
                "claim_fallback_direct:wrote",
            );
            tracing::info!(
                event = "claim_fallback_direct",
                display_id = %display_label,
                result = "wrote",
            );
        }
    }

    fn feed_requester_failed(&mut self, display: &DisplayId, failure: ClaimFailure) {
        self.record_event("claim_failed");
        append_event(
            self.event_log.as_ref(),
            self.event_notify.as_ref(),
            format!("claim_failed:{}", display.0),
        );
        append_event(
            self.event_log.as_ref(),
            self.event_notify.as_ref(),
            format!("reason:{failure:?}"),
        );
    }

    fn local_identity_view(&self) -> dormant_core::peers::InstanceIdentity {
        // Cheap clone of the local identity (SigningKey is
        // internally Arc-like — Clone is cheap and the runtime
        // owns its own Arc anyway).
        dormant_core::peers::InstanceIdentity {
            instance_id: self.local_instance_id.clone(),
            signing_key: self.local_signing.clone(),
            verifying_key: self.local_signing.verifying_key(),
        }
    }

    fn nonce_of(&self, display: &DisplayId) -> String {
        self.flights
            .get(display)
            .map(|f| f.nonce.clone())
            .unwrap_or_default()
    }

    fn find_display_by_claim_identity(&self, identity: &str) -> Option<DisplayId> {
        self.contexts.iter().find_map(|(id, ctx)| {
            if ctx.claim_identity.as_deref() == Some(identity) {
                Some(id.clone())
            } else {
                None
            }
        })
    }

    fn find_display_by_requester_nonce(&self, nonce: &str) -> Option<DisplayId> {
        self.flights
            .iter()
            .find_map(|(id, f)| (f.nonce == nonce).then_some(id.clone()))
    }

    fn find_display_by_owner_nonce(&self, nonce: &str) -> Option<DisplayId> {
        // Owner side: the nonce is the request's nonce (the
        // requester copied it; we copied it into the flight
        // record when the request arrived).
        self.flights
            .iter()
            .find_map(|(id, f)| (f.nonce == nonce).then_some(id.clone()))
    }

    async fn refresh_contexts_from_config(&mut self) {
        let cfg = self.config.borrow().clone();
        let executors = self.executors.borrow().clone();
        for (name, display_config) in &cfg.displays {
            if display_config.scope != dormant_core::config::DisplayScope::Shared {
                continue;
            }
            let id = DisplayId(name.clone());
            let Some(sink) = executors.get(&id) else {
                info!(event = "claim_context_skipped", reason = "no_executor", display_id = %id.0);
                continue;
            };
            let writable = sink_input_writable(sink.clone()).await.is_not_incapable();
            let claim_identity = sink.claim_identity();
            let hooks = Arc::new(display_config.hooks.clone());
            let ctx = DisplayCtx {
                local_input_code: display_config.shared_input_code,
                local_input_write_code: display_config.shared_input_write_code,
                claim_identity,
                writable,
                hooks,
            };
            self.contexts.insert(id.clone(), ctx);
            self.engines.entry(id.clone()).or_insert_with(|| {
                ClaimEngine::with_release_policy(
                    cfg.coordination.poll_interval,
                    cfg.coordination.release_deadline_cap,
                )
            });
        }
        let removed: Vec<DisplayId> = self
            .contexts
            .keys()
            .filter(|id| !cfg.displays.contains_key(&id.0))
            .cloned()
            .collect();
        for id in removed {
            self.contexts.remove(&id);
            self.engines.remove(&id);
            self.flights.remove(&id);
        }
        // Refresh the snapshot-side fields.
        let claim_capable: Vec<DisplayId> = self
            .contexts
            .iter()
            .filter_map(|(id, ctx)| {
                if ctx.writable && ctx.claim_identity.is_some() {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        let _ = claim_capable;
    }

    fn release_deadline_cap(&self) -> Duration {
        self.config.borrow().coordination.release_deadline_cap
    }

    fn claim_timeout(&self) -> Duration {
        self.config.borrow().coordination.claim_timeout
    }

    fn armed_window(&self) -> Duration {
        self.config.borrow().coordination.armed_window
    }

    fn build_claim_request(
        &self,
        display: &DisplayId,
        nonce: &str,
    ) -> Option<(u64, String, ClaimMessage)> {
        let ctx = self.contexts.get(display)?;
        // Use the write code when present (some panels accept a different
        // value on `setvcp 60` than they report on `getvcp 60`); the owner
        // writes this code, so the requester must send the write code on
        // the wire.
        let write_code = u16::from(ctx.effective_write_code()?);
        let request = ClaimRequest {
            display_identity: ctx.claim_identity.clone()?,
            requester_instance_id: self.local_instance_id.clone(),
            requester_input_code: write_code,
            counter: self.outbound_counter,
            nonce: nonce.to_owned(),
        };
        Some((
            self.outbound_counter,
            nonce.to_owned(),
            ClaimMessage::ClaimRequest(request),
        ))
    }

    fn next_counter(&mut self) -> u64 {
        self.outbound_counter = self.outbound_counter.saturating_add(1);
        self.outbound_counter
    }

    fn next_nonce(&mut self) -> String {
        let nanos = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos(),
            Err(_) => 0,
        };
        let nonce = format!("req-{nanos}-{}", self.outbound_counter);
        self.outbound_nonces.push_back(nonce.clone());
        if self.outbound_nonces.len() > 64 {
            self.outbound_nonces.pop_front();
        }
        nonce
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// Sum blocking `before_release` timeouts from a hook snapshot.
fn sum_blocking_before_release(hooks: &HookSlots) -> Duration {
    hooks
        .before_release
        .iter()
        .filter(|action| {
            action
                .blocking
                .unwrap_or(crate::hooks::default_blocking_for(
                    crate::hooks::Phase::Before,
                ))
        })
        .map(|action| action.timeout)
        .sum()
}

/// Probe-time writability check with three-way outcome.
///
/// Reads the current input-source code at sampler priority, then
/// writes it back (a no-op that exercises the write surface). A
/// sampler-priority skip (`INPUT_SOURCE_SKIPPED` — the coordination
/// poller held the panel lock) is **not** a capability signal:
/// it means the probe could not run, not that the display lacks
/// writability. The real claim path uses command priority and
/// completes even when this probe is inconclusive.
///
/// # Return
///
/// * [`InputWritability::Capable`] — read succeeded, write succeeded.
/// * [`InputWritability::Incapable`] — the controller definitively
///   returned `INPUT_SOURCE_WRITE_UNSUPPORTED`.
/// * [`InputWritability::Unknown`] — sampler skipped, transient I/O
///   error, or no controller reported an input-source code.
pub(crate) async fn sink_input_writable(sink: Arc<dyn CommandSink>) -> InputWritability {
    // Use the display name from the claim identity for logging.
    let display_label = sink
        .claim_identity()
        .unwrap_or_else(|| "unknown".to_string());

    let observed = match sink.read_input_source_sampled().await {
        Ok(Some(code)) => code,
        Ok(None) => {
            // No controller in the chain reports an input-source
            // code. This is structural (e.g. command-only chain),
            // not transient — but it is not definitive for
            // writability either (the executor may have a fallback
            // controller that does not support readback). Treat
            // as unknown so the display is not permanently excluded.
            info!(
                event = "claim_capability_probed",
                display = %display_label,
                writable = "unknown",
                reason = "no controller reports input-source code",
            );
            return InputWritability::Unknown;
        }
        Err(ref e) if e.contains("command holds panel lock") => {
            // Sampler priority yielded to a concurrent
            // command-path transaction (coordination poller or
            // another caller). The display IS DDC-capable — we
            // just lost the race. Mark unknown, not incapable.
            info!(
                event = "claim_capability_probed",
                display = %display_label,
                writable = "unknown",
                reason = "sampler skipped: command holds panel lock",
            );
            return InputWritability::Unknown;
        }
        Err(ref e) => {
            // Transient I/O error (display disconnected, DDC bus
            // flaky, etc.). Unknown — re-probe next generation.
            info!(
                event = "claim_capability_probed",
                display = %display_label,
                writable = "unknown",
                reason = %e,
            );
            return InputWritability::Unknown;
        }
    };

    // Read succeeded — now probe the write surface.
    match sink
        .write_input_source(InputSourceTarget {
            write_code: observed,
            expected_readback: InputSourceReadback::Exact(observed),
        })
        .await
    {
        Ok(()) => {
            info!(
                event = "claim_capability_probed",
                display = %display_label,
                writable = "capable",
                observed_input_code = observed,
            );
            InputWritability::Capable
        }
        Err(ref failure) if failure.error.contains("unsupported input-source write") => {
            info!(
                event = "claim_input_source_not_writable",
                display = %display_label,
                reason = "controller returned unsupported input-source write",
            );
            InputWritability::Incapable
        }
        Err(ref failure) => {
            // Write failed with an I/O error (not "unsupported").
            // Sampler already confirmed the display is reachable;
            // this is likely transient — mark unknown.
            info!(
                event = "claim_capability_probed",
                display = %display_label,
                writable = "unknown",
                reason = %failure.error,
                observed_input_code = observed,
            );
            InputWritability::Unknown
        }
    }
}

/// Synchronous claim-capable check (suitable for snapshot-time
/// fold). Returns `true` when the sink exposes a F5 claim identity
/// (the strongest static signal without I/O — the runtime's own
/// per-display engine does a real probe before driving claims).
/// `writability` is the runtime's view; this helper covers the
/// identity side.
pub fn sink_claim_capable(sink: &Arc<dyn CommandSink>) -> bool {
    sink.claim_identity().is_some()
}

/// `HookSlots` direction/phase accessor for the runtime.
trait HookSlotsExt {
    fn slot_for(&self, direction: Direction, phase: Phase) -> &[HookAction];
}

impl HookSlotsExt for HookSlots {
    fn slot_for(&self, direction: Direction, phase: Phase) -> &[HookAction] {
        match (direction, phase) {
            (Direction::Release, Phase::Before) => &self.before_release,
            (Direction::Release, Phase::After) => &self.after_release,
            (Direction::Acquire, Phase::Before) => &self.before_acquire,
            (Direction::Acquire, Phase::After) => &self.after_acquire,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookRunner, ScriptedHookRunner};
    use dormant_core::claim_engine::Terminal;
    use dormant_core::rules::ControllerHealth;
    use dormant_core::types::{BlankMode, CmdFailure};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    /// Validate the literal claim lifecycle vocabulary is
    /// exactly the thirteen-anchor set the spec mandates.
    #[test]
    fn claim_events_vocabulary_is_exact() {
        assert_eq!(
            CLAIM_EVENTS,
            [
                "claim_requested",
                "claim_accepted",
                "claim_denied",
                "claim_busy",
                "claim_not_owner",
                "claim_release_aborted",
                "claim_release_failed",
                "claim_fallback_direct",
                "claim_failed",
                "claim_completed",
                "claim_acquire_failed",
                "claim_acquire_ready",
                "claim_acquire_wait_expired",
            ]
        );
    }

    #[test]
    fn reply_frames_verify_against_verified_peer_epoch() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use dormant_core::peers::{PeerRecord, instance_id_from_public_key};

        let sender_signing = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
        let sender = InstanceIdentity {
            instance_id: instance_id_from_public_key(&sender_signing.verifying_key().to_bytes()),
            verifying_key: sender_signing.verifying_key(),
            signing_key: sender_signing,
        };
        let recipient_signing = ed25519_dalek::SigningKey::from_bytes(&[22; 32]);
        let recipient_id =
            instance_id_from_public_key(&recipient_signing.verifying_key().to_bytes());
        let sender_record = PeerRecord {
            instance_id: sender.instance_id.clone(),
            ed25519_pub: STANDARD.encode(sender.verifying_key.to_bytes()),
            display_name: "sender".to_owned(),
            paired_at: "2026-01-01T00:00:00Z".to_owned(),
            last_addr: None,
            claim_port: None,
        };
        let messages = [
            ClaimMessage::ClaimAbort(ClaimAbort {
                nonce: "abort".to_owned(),
            }),
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce: "response".to_owned(),
                verdict: ClaimVerdict::Accepted { eta_ms: 10 },
            }),
            ClaimMessage::ReleaseFailed(ReleaseFailed {
                nonce: "release".to_owned(),
                reason: "write failed".to_owned(),
            }),
        ];

        for (counter, message) in messages.into_iter().enumerate() {
            let frame = sign_frame_for_peer(
                &sender,
                "sender-epoch-000".to_owned(),
                recipient_id.clone(),
                "peer-epoch-00001".to_owned(),
                counter as u64 + 1,
                format!("frame-{counter}"),
                message,
            )
            .expect("reply frame signs");

            frame
                .verify(&sender_record, &recipient_id, "peer-epoch-00001")
                .expect("peer accepts reply addressed to its verified epoch");
            assert_ne!(frame.recipient_epoch, "sender-epoch-000");
        }
    }

    #[test]
    fn zero_contact_fanout_is_not_negotiated() {
        let no_contact = crate::coordination_claim::FanoutResult {
            contacted: 0,
            dial_failed: 2,
            ..crate::coordination_claim::FanoutResult::default()
        };
        let contacted = crate::coordination_claim::FanoutResult {
            contacted: 2,
            ..crate::coordination_claim::FanoutResult::default()
        };

        assert_eq!(negotiated_peer_count(no_contact), None);
        assert_eq!(negotiated_peer_count(contacted), Some(2));
    }

    /// The driver's `KvmStatus` is the post-probe fold: an empty
    /// fresh handle returns the default (no claim-capable
    /// displays, no keymap, default policy) — covers the public
    /// shape the IPC layer relies on.
    #[test]
    fn kvm_status_defaults_roundtrip() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<RuntimeEvent>(1);
        let handle = ClaimRuntimeHandle {
            cmd_tx,
            armed: Arc::new(Mutex::new(HashMap::new())),
            claim_capable: Arc::new(Mutex::new(Vec::new())),
            keymap: Arc::new(Mutex::new(KeymapConfig::default())),
            activity_claim: Arc::new(Mutex::new(ActivityClaimPolicy::Off)),
            release_deadline_cap: Arc::new(Mutex::new(Duration::from_secs(45))),
            suppressed: Arc::new(Mutex::new(HashMap::new())),
        };
        let status = handle.kvm_status();
        assert!(status.claim_capable_displays.is_empty());
        assert_eq!(status.activity_claim, ActivityClaimPolicy::Off);
    }

    /// `sum_blocking_before_release` honors the per-entry
    /// `blocking` override and the `before_*` default. Used by
    /// the owner-side `eta` computation.
    #[test]
    fn blocking_before_release_sums_only_blocking_entries() {
        use dormant_core::config::schema::HookAction;
        let blocking = HookAction {
            command: Some(vec!["echo".to_string()]),
            mqtt: None,
            timeout: Duration::from_secs(2),
            blocking: Some(true),
            abort_on_failure: false,
        };
        let non_blocking = HookAction {
            command: Some(vec!["echo".to_string()]),
            mqtt: None,
            timeout: Duration::from_secs(2),
            blocking: Some(false),
            abort_on_failure: false,
        };
        let default_blocking = HookAction {
            command: Some(vec!["echo".to_string()]),
            mqtt: None,
            timeout: Duration::from_secs(3),
            blocking: None,
            abort_on_failure: false,
        };
        let slots = HookSlots {
            before_release: vec![blocking, non_blocking, default_blocking],
            after_release: vec![],
            before_acquire: vec![],
            after_acquire: vec![],
        };
        // blocking(2s) + default(3s) = 5s
        assert_eq!(sum_blocking_before_release(&slots), Duration::from_secs(5));
    }

    /// Verify a `ScriptedHookRunner` records the calls and
    /// returns the queued responses. This is the test seam the
    /// runtime's `HookEngine` uses.
    #[tokio::test]
    async fn scripted_hook_runner_records_command_argvs() {
        let runner = ScriptedHookRunner::new();
        runner.push_command(Ok(()));
        let outcome = runner
            .run_command(
                &Vec::new(),
                &["notify-send".to_string(), "released".to_string()],
                Duration::from_secs(1),
            )
            .await;
        assert!(outcome.is_ok());
        let recorded = runner.command_argvs();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0],
            vec!["notify-send".to_string(), "released".to_string()]
        );
    }

    /// Pure-engine fallback contract: when the requester
    /// deadline elapses with no peer accepted, the engine
    /// emits `SendAbort` + `Trace("claim_fallback_direct")` +
    /// `AttemptFallback`. The driver routes the fallback's
    /// fresh read + write decision. This test pins the
    /// engine's action sequence so the dispatch contract is
    /// stable.
    #[test]
    fn fallback_action_sequence_is_sendabort_trace_attempt() {
        let mut engine = ClaimEngine::default();
        let now = Instant::now();
        engine.begin_requester(
            DisplayId("mon".into()),
            "n",
            1,
            now,
            Duration::from_millis(50),
        );
        engine.requester_event(&DisplayId("mon".into()), RequesterEvent::FanoutSent, now);
        // Deadline expires in AwaitingAck → RequesterUnaccepted fallback.
        let actions = engine.on_deadline(&DisplayId("mon".into()), now + Duration::from_secs(60));
        let has_send_abort = actions.iter().any(|a| matches!(a, Action::SendAbort));
        let has_attempt = actions.iter().any(|a| matches!(a, Action::AttemptFallback));
        let has_trace = actions
            .iter()
            .any(|a| matches!(a, Action::Trace("claim_fallback_direct")));
        assert!(has_send_abort, "fallback must include SendAbort");
        assert!(has_attempt, "fallback must include AttemptFallback");
        assert!(
            has_trace,
            "fallback must include claim_fallback_direct trace"
        );
    }

    /// Pure-engine Busy on concurrent local trigger: a second
    /// `begin_requester` while the first is in flight emits
    /// `claim_busy` + `BusyLocal`.
    #[test]
    fn concurrent_local_trigger_emits_busy() {
        let mut engine = ClaimEngine::default();
        let now = Instant::now();
        let first = engine.begin_requester(
            DisplayId("mon".into()),
            "first",
            1,
            now,
            Duration::from_secs(3),
        );
        assert!(
            first
                .iter()
                .any(|a| matches!(a, Action::Trace("claim_requested")))
        );
        let second = engine.begin_requester(
            DisplayId("mon".into()),
            "second",
            1,
            now,
            Duration::from_secs(3),
        );
        assert!(second.iter().any(|a| matches!(a, Action::BusyLocal)));
        assert!(
            second
                .iter()
                .any(|a| matches!(a, Action::Trace("claim_busy")))
        );
    }

    /// Display-removed mid-claim lifts the requester flight
    /// with `claim_failed` + `Terminal::Removed` and clears
    /// the F10 suppression.
    #[test]
    fn display_removed_lifts_requester_flight() {
        let mut engine = ClaimEngine::default();
        let now = Instant::now();
        engine.begin_requester(DisplayId("mon".into()), "n", 1, now, Duration::from_secs(3));
        engine.requester_event(&DisplayId("mon".into()), RequesterEvent::FanoutSent, now);
        engine.requester_event(
            &DisplayId("mon".into()),
            RequesterEvent::Response {
                nonce: "n".to_owned(),
                peer_instance_id: "peer".to_owned(),
                verdict: ClaimVerdict::Accepted { eta_ms: 5_000 },
            },
            now,
        );
        // Requester is in Watching. DisplayRemoved terminalises.
        let actions = engine.requester_event(
            &DisplayId("mon".into()),
            RequesterEvent::DisplayRemoved,
            now,
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Terminal(Terminal::Removed)))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Trace("claim_failed")))
        );
        // F10 lifts: the requester flight is gone.
        assert!(!engine.is_suppressed(&DisplayId("mon".into()), now));
    }

    /// Owner side: a hook-aborted `before_release` lifts
    /// the flight with `claim_release_aborted` + a
    /// `ReleaseFailed` best-effort notification.
    #[test]
    fn owner_hook_aborted_lifts_with_release_aborted() {
        let mut engine = ClaimEngine::default();
        let now = Instant::now();
        let request = OwnerRequest {
            display: DisplayId("mon".into()),
            requested_identity: "id".to_owned(),
            local_identity: Some("id".to_owned()),
            requester_input_code: 0x11,
            local_input_code: 0x0f,
            capability: ClaimCapability::Writable,
            disposition: OwnerDisposition::Ready { standby: false },
            eta: Duration::from_secs(1),
        };
        let actions = engine.begin_owner(request, "n", now, Duration::from_secs(45));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Trace("claim_accepted")))
        );
        // Drive the engine through AckSent → WaitForAcquireReady → BeforeRelease.
        let _ack_actions = engine.owner_event(&DisplayId("mon".into()), OwnerEvent::AckDelivered);
        let _ready_actions = engine.owner_event(&DisplayId("mon".into()), OwnerEvent::AcquireReady);
        // The engine's BeforeRelease slot now expects the
        // hook outcome. The hook aborted.
        let actions = engine.owner_event(
            &DisplayId("mon".into()),
            OwnerEvent::BeforeRelease(HookResult::Aborted),
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Trace("claim_release_aborted"))),
            "hook-aborted owner emits claim_release_aborted; got: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::SendReleaseFailed)),
            "hook-aborted owner emits SendReleaseFailed; got: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Terminal(Terminal::ReleaseAborted))),
            "hook-aborted owner reaches Terminal::ReleaseAborted; got: {actions:?}"
        );
    }

    // ── InputWritability probe tests ─────────────────────────────────────

    /// Minimal [`CommandSink`] double that serves scripted
    /// input-source read and write results for testing
    /// [`sink_input_writable`].
    struct ProbeSink {
        claim_id: Option<String>,
        read_result: Result<Option<u8>, String>,
        write_result: Result<(), CmdFailure>,
    }

    impl ProbeSink {
        fn with_read(v: Result<Option<u8>, String>) -> Self {
            Self {
                claim_id: Some("MFG:MODEL:SN".into()),
                read_result: v,
                write_result: Ok(()),
            }
        }

        fn with_read_and_write(r: Result<Option<u8>, String>, w: Result<(), CmdFailure>) -> Self {
            Self {
                claim_id: Some("MFG:MODEL:SN".into()),
                read_result: r,
                write_result: w,
            }
        }
    }

    #[async_trait::async_trait]
    impl CommandSink for ProbeSink {
        async fn blank(&self, _mode: BlankMode) -> Result<(), CmdFailure> {
            Ok(())
        }

        async fn wake(&self) -> Result<(), CmdFailure> {
            Ok(())
        }

        fn controller_health(&self) -> Vec<ControllerHealth> {
            Vec::new()
        }

        fn claim_identity(&self) -> Option<String> {
            self.claim_id.clone()
        }

        async fn read_input_source_sampled(&self) -> Result<Option<u8>, String> {
            self.read_result.clone()
        }

        async fn write_input_source(&self, _target: InputSourceTarget) -> Result<(), CmdFailure> {
            self.write_result.clone()
        }
    }

    /// Sampler lock skip (`INPUT_SOURCE_SKIPPED`) → `Unknown`,
    /// NOT `Incapable`. The display is DDC-capable; we just lost
    /// the race for the panel lock.
    ///
    /// Mutation: if this test passes and a later change collapses
    /// `Unknown` back to `Incapable`, `is_not_incapable()` would
    /// return `false` and claims would be permanently denied.
    #[tokio::test]
    async fn sampler_skip_does_not_mark_display_incapable() {
        let skip_err = "skipped: command holds panel lock".to_string();
        let sink = Arc::new(ProbeSink::with_read(Err(skip_err)));
        let verdict = sink_input_writable(sink).await;
        assert_eq!(
            verdict,
            InputWritability::Unknown,
            "sampler skip must be Unknown, not Incapable"
        );
        assert!(
            verdict.is_not_incapable(),
            "Unknown.is_not_incapable() must be true; claims must not be blocked"
        );
    }

    /// Genuine `INPUT_SOURCE_WRITE_UNSUPPORTED` → `Incapable`.
    /// This path must still work: a controller that cannot write
    /// input source is definitively incapable.
    #[tokio::test]
    async fn unsupported_write_marks_display_incapable() {
        let sink = Arc::new(ProbeSink::with_read_and_write(
            Ok(Some(0x0f)),
            Err(CmdFailure {
                controller: "ddcci".into(),
                error: "E_DISPLAY_IO: unsupported input-source write".into(),
            }),
        ));
        let verdict = sink_input_writable(sink).await;
        assert_eq!(
            verdict,
            InputWritability::Incapable,
            "unsupported write must be Incapable"
        );
        assert!(
            !verdict.is_not_incapable(),
            "Incapable.is_not_incapable() must be false"
        );
    }

    /// Successful read + write → `Capable`.
    #[tokio::test]
    async fn successful_read_and_write_returns_capable() {
        let sink = Arc::new(ProbeSink::with_read(Ok(Some(0x0f))));
        let verdict = sink_input_writable(sink).await;
        assert_eq!(
            verdict,
            InputWritability::Capable,
            "successful read+write must be Capable"
        );
        assert!(verdict.is_not_incapable());
    }

    /// Transient I/O error on read → `Unknown`, not `Incapable`.
    #[tokio::test]
    async fn transient_io_error_on_read_returns_unknown() {
        let sink = Arc::new(ProbeSink::with_read(
            Err("E_DISPLAY_IO: read failed".into()),
        ));
        let verdict = sink_input_writable(sink).await;
        assert_eq!(
            verdict,
            InputWritability::Unknown,
            "transient I/O error must be Unknown"
        );
        assert!(verdict.is_not_incapable());
    }

    /// `Ok(None)` (no input-source readback) → `Unknown`.
    /// See the comment in `sink_input_writable` — a controller
    /// chain without readback is not definitive for writability.
    #[tokio::test]
    async fn no_input_source_readback_returns_unknown() {
        let sink = Arc::new(ProbeSink::with_read(Ok(None)));
        let verdict = sink_input_writable(sink).await;
        assert_eq!(
            verdict,
            InputWritability::Unknown,
            "no readback must be Unknown"
        );
        assert!(verdict.is_not_incapable());
    }

    /// Write fails with transient I/O error (not "unsupported")
    /// → `Unknown`. The sampler already confirmed the display is
    /// reachable; the write failure is likely transient.
    #[tokio::test]
    async fn transient_write_error_returns_unknown() {
        let sink = Arc::new(ProbeSink::with_read_and_write(
            Ok(Some(0x0f)),
            Err(CmdFailure {
                controller: "ddcci".into(),
                error: "E_DISPLAY_IO: i2c timeout".into(),
            }),
        ));
        let verdict = sink_input_writable(sink).await;
        assert_eq!(
            verdict,
            InputWritability::Unknown,
            "transient write error must be Unknown"
        );
        assert!(verdict.is_not_incapable());
    }

    // ── Bug B: terminal outcome tracing emissions ───────────────────

    /// Every terminal outcome in the fallback path emits its
    /// literal anchor to `tracing`.  Uses `tracing_subscriber`
    /// `fmt` collector with a shared buffer so we can inspect
    /// the actual output.
    ///
    /// **Mutation:** remove a `tracing::info!` from
    /// `run_direct_fallback` → this test fails because the
    /// captured output no longer contains the anchor.
    #[tokio::test]
    async fn terminal_fallback_outcomes_emit_tracing_anchors() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = MakeTestWriter(Arc::clone(&buffer));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // Test the "already on target" path (claim_completed).
        {
            let sink: Arc<dyn CommandSink> = Arc::new(ProbeSink::with_read(Ok(Some(0x11))));
            let notify = Arc::new(Notify::new());
            super::Driver::run_direct_fallback(
                sink,
                0x11, // target == observed → already_on_target
                "test-display".into(),
                Some(Arc::new(Mutex::new(Vec::<String>::new()))),
                Some(notify),
            )
            .await;
            let output = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
            assert!(
                output.contains("claim_completed"),
                "claim_completed tracing anchor missing; output: {output}"
            );
            assert!(
                output.contains("already_on_target"),
                "already_on_target reason missing; output: {output}"
            );
            buffer.lock().unwrap().clear();
        }

        // Test the write path (claim_failed:write → claim_failed).
        {
            let sink: Arc<dyn CommandSink> = Arc::new(ProbeSink::with_read_and_write(
                Ok(Some(0x22)),
                Err(CmdFailure {
                    controller: "test".into(),
                    error: "fail".into(),
                }),
            ));
            let notify = Arc::new(Notify::new());
            super::Driver::run_direct_fallback(
                sink,
                0x99, // target != observed
                "test-display".into(),
                Some(Arc::new(Mutex::new(Vec::<String>::new()))),
                Some(notify),
            )
            .await;
            let output = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
            assert!(
                output.contains("claim_failed"),
                "claim_failed tracing anchor missing; output: {output}"
            );
            assert!(
                output.contains("write"),
                "reason=write missing; output: {output}"
            );
        }
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the transport and runtime seam is assembled in one regression test"
    )]
    async fn real_transport_forwards_verified_request_to_runtime() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        use crate::coordination_claim::{ClaimPeer, ClaimTransportDeps};
        use crate::coordination_frame::{read_frame, write_frame};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use dormant_core::claim::Epoch;
        use dormant_core::config::{Strictness, load_config_from_str};
        use dormant_core::peers::{PeerRecord, instance_id_from_public_key};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::watch as tokio_watch;

        const REQUESTER_EPOCH: &str = "request-epoch-01";
        const OWNER_EPOCH: &str = "owner-boot-00001";
        let identity = |seed: u8| {
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
            InstanceIdentity {
                instance_id: instance_id_from_public_key(&signing_key.verifying_key().to_bytes()),
                verifying_key: signing_key.verifying_key(),
                signing_key,
            }
        };
        let requester = identity(31);
        let owner = identity(32);
        let response_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let response_port = response_listener.local_addr().unwrap().port();
        let (_peers_tx, peers_rx) = tokio_watch::channel(vec![ClaimPeer {
            instance_id: requester.instance_id.clone(),
            verifying_key: requester.verifying_key,
            last_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, response_port))),
            dns_addr: None,
            claim_port: Some(response_port),
            dns_port: None,
            dns_epoch: Some(Epoch::try_from(REQUESTER_EPOCH).unwrap()),
        }]);
        let transport = Arc::new(crate::coordination_claim::spawn(ClaimTransportDeps {
            identity: Arc::new(owner.clone()),
            boot_epoch: Epoch::try_from(OWNER_EPOCH).unwrap(),
            peers: peers_rx,
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            fixed_port: None,
            enabled: true,
            on_peer_addr: Box::new(|_, _| {}),
        }));
        let owner_port = transport.ensure_provisional_listener().await.unwrap();

        let (config, _) = load_config_from_str("config_version = 1\n", Strictness::Warn).unwrap();
        let (_config_tx, config_rx) = tokio_watch::channel(Arc::new(config));
        let (_executors_tx, executors_rx) = tokio_watch::channel(Arc::new(HashMap::new()));
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let _runtime = spawn(ClaimRuntimeDeps {
            identity: Arc::new(owner.clone()),
            transport: Arc::clone(&transport),
            executors: executors_rx,
            config: config_rx,
            hooks: Arc::new(HookEngine::with_runner(Arc::new(ScriptedHookRunner::new()))),
            coordination: None,
            front_ctl_tx,
            cancel: cancel.clone(),
            event_log: None,
            event_notify: None,
            idle_rx: None,
        });
        let response_task = tokio::spawn(async move {
            let (mut stream, _) = response_listener.accept().await.unwrap();
            read_frame::<ClaimFrame, _>(&mut stream).await.unwrap()
        });

        let request_nonce = "runtime-e2e-request";
        let request = ClaimRequest {
            display_identity: "unknown-display".to_owned(),
            requester_instance_id: requester.instance_id.clone(),
            requester_input_code: 0x11,
            counter: 1,
            nonce: request_nonce.to_owned(),
        };
        let request_frame = ClaimFrame::sign(
            &requester,
            REQUESTER_EPOCH.to_owned(),
            owner.instance_id.clone(),
            OWNER_EPOCH.to_owned(),
            1,
            request_nonce.to_owned(),
            ClaimMessage::ClaimRequest(request),
        )
        .unwrap();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, owner_port))
            .await
            .unwrap();
        write_frame(&mut stream, &request_frame).await.unwrap();

        let response = tokio::time::timeout(Duration::from_secs(2), response_task)
            .await
            .expect("real transport request reaches runtime")
            .unwrap();
        let owner_record = PeerRecord {
            instance_id: owner.instance_id.clone(),
            ed25519_pub: STANDARD.encode(owner.verifying_key.to_bytes()),
            display_name: "owner".to_owned(),
            paired_at: "2026-01-01T00:00:00Z".to_owned(),
            last_addr: None,
            claim_port: None,
        };
        response
            .verify(&owner_record, &requester.instance_id, REQUESTER_EPOCH)
            .expect("requester accepts owner reply");
        assert!(matches!(
            response.message,
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce,
                verdict: ClaimVerdict::NotOwner,
            }) if nonce == request_nonce
        ));

        cancel.cancel();
        transport.shutdown().await;
    }

    /// `MakeWriter` implementation that writes to a shared buffer.
    struct MakeTestWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeTestWriter {
        type Writer = TestWriter;

        fn make_writer(&'a self) -> Self::Writer {
            TestWriter {
                buffer: Arc::clone(&self.0),
            }
        }
    }

    struct TestWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut b) = self.buffer.lock() {
                b.extend_from_slice(buf);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A signed `ClaimRequest` built with identical envelope and message
    /// nonces must pass `ClaimFrame::verify` on the recipient side.
    /// The mutation arm reintroduces the double-prefix so a regression
    /// is caught immediately.
    #[test]
    fn claim_request_nonce_matches_envelope() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use dormant_core::peers::{PeerRecord, instance_id_from_public_key};

        let signing = ed25519_dalek::SigningKey::from_bytes(&[99; 32]);
        let identity = InstanceIdentity {
            instance_id: instance_id_from_public_key(&signing.verifying_key().to_bytes()),
            signing_key: signing,
            verifying_key: ed25519_dalek::SigningKey::from_bytes(&[99; 32]).verifying_key(),
        };
        let request = ClaimRequest {
            display_identity: "edid:test".to_owned(),
            requester_instance_id: identity.instance_id.clone(),
            requester_input_code: 15,
            counter: 42,
            nonce: "req-123-42".to_owned(),
        };
        // The fixed path: envelope nonce == request.nonce (both "req-…").
        let frame = ClaimFrame::sign(
            &identity,
            "sender-epoch-000".to_owned(),
            "recipient-id".to_owned(),
            "recipient-epoch-".to_owned(),
            42,
            "req-123-42".to_owned(),
            ClaimMessage::ClaimRequest(request),
        )
        .unwrap();
        let peer = PeerRecord {
            instance_id: identity.instance_id.clone(),
            ed25519_pub: STANDARD.encode(identity.verifying_key.as_bytes()),
            display_name: String::new(),
            paired_at: String::new(),
            last_addr: None,
            claim_port: None,
        };
        frame
            .verify(&peer, "recipient-id", "recipient-epoch-")
            .expect("verify must pass when request.nonce == frame.nonce");

        // Mutation: reintroduce the double-prefix (the old bug).
        let bad_request = ClaimRequest {
            display_identity: "edid:test".to_owned(),
            requester_instance_id: identity.instance_id.clone(),
            requester_input_code: 15,
            counter: 42,
            nonce: "req-123-42".to_owned(),
        };
        let bad_frame = ClaimFrame::sign(
            &identity,
            "sender-epoch-000".to_owned(),
            "recipient-id".to_owned(),
            "recipient-epoch-".to_owned(),
            42,
            "req-req-123-42".to_owned(),
            ClaimMessage::ClaimRequest(bad_request),
        )
        .unwrap();
        let err = bad_frame
            .verify(&peer, "recipient-id", "recipient-epoch-")
            .expect_err("mismatched nonces must be rejected");
        assert_eq!(
            err,
            dormant_core::claim::ClaimFrameError::RequestReplayMismatch
        );
    }

    /// When the executor map is populated AFTER the runtime starts,
    /// the `executors.changed()` arm must refresh contexts so a
    /// subsequent claim can find a writable context.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn executors_changed_refreshes_contexts() {
        use crate::coordination_claim;
        use std::net::{IpAddr, Ipv4Addr};
        use tokio::sync::watch;
        use tokio_util::sync::CancellationToken;

        let display = DisplayId("panel".to_owned());
        let cancel = CancellationToken::new();
        let (executors_tx, executors_rx) =
            watch::channel(Arc::new(HashMap::<DisplayId, Arc<dyn CommandSink>>::new()));
        let (_config_tx, config_rx) = watch::channel({
            let mut displays = indexmap::IndexMap::new();
            displays.insert(
                "panel".to_owned(),
                dormant_core::config::DisplayConfig {
                    controllers: vec!["cmd".to_owned()],
                    scope: dormant_core::config::DisplayScope::Shared,
                    shared_input_code: Some(0x0f),
                    shared_input_write_code: None,
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
                    hooks: HookSlots::default(),
                },
            );
            Arc::new(Config {
                config_version: 1,
                daemon: dormant_core::config::DaemonConfig::default(),
                sensors: indexmap::IndexMap::new(),
                zones: indexmap::IndexMap::new(),
                displays,
                rules: indexmap::IndexMap::new(),
                wear: dormant_core::config::schema::WearConfig::default(),
                notifications: dormant_core::config::schema::NotificationsConfig::default(),
                watchdog: dormant_core::config::schema::WatchdogConfig::default(),
                audio: dormant_core::config::schema::AudioConfig::default(),
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
                },
            })
        });
        let identity = {
            let signing = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
            InstanceIdentity {
                instance_id: dormant_core::peers::instance_id_from_public_key(
                    &signing.verifying_key().to_bytes(),
                ),
                signing_key: signing,
                verifying_key: ed25519_dalek::SigningKey::from_bytes(&[42; 32]).verifying_key(),
            }
        };
        let transport = {
            use dormant_core::claim::Epoch;
            let deps = coordination_claim::ClaimTransportDeps {
                identity: Arc::new(identity.clone()),
                boot_epoch: Epoch::try_from("0123456789abcdef").unwrap(),
                peers: watch::channel(Vec::new()).1,
                bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                fixed_port: None,
                enabled: true,
                on_peer_addr: Box::new(|_, _| {}),
            };
            coordination_claim::spawn(deps)
        };
        let coord = CoordinationHandle::new([display.clone()]);
        coord.record_success(&display, 0x0f, 0x0f, None);
        let (front_ctl_tx, _front_ctl_rx) = mpsc::channel(8);
        let handle = spawn(ClaimRuntimeDeps {
            identity: Arc::new(identity),
            transport: transport.into(),
            executors: executors_rx,
            config: config_rx,
            hooks: Arc::new(crate::hooks::HookEngine::with_runner(
                Arc::new(ScriptedHookRunner::new()) as Arc<dyn HookRunner>,
            )),
            coordination: Some(coord),
            front_ctl_tx,
            cancel: cancel.clone(),
            event_log: None,
            event_notify: None,
            idle_rx: None,
        });

        // Start with an empty executor map: the claim must be
        // denied (no context).
        let before = handle.try_claim(display.clone()).await.expect("try_claim");
        assert!(
            matches!(
                before,
                ClaimSharedResult::Denied(ClaimDeniedReason::Unsupported)
            ),
            "empty executor map must deny; got {before:?}"
        );

        // Populate the executor map — the claim runtime's
        // `executors.changed()` arm must refresh contexts so a
        // subsequent claim finds a writable context.
        let sink: Arc<dyn CommandSink> = Arc::new(ProbeSink::with_read(Ok(Some(0x0f))));
        let mut map = HashMap::new();
        map.insert(display.clone(), sink);
        let _ = executors_tx.send(Arc::new(map));
        // Give the runtime a moment to process the notification.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let after = handle
            .try_claim(display.clone())
            .await
            .expect("try_claim after executors populated");
        assert!(
            matches!(
                after,
                ClaimSharedResult::Accepted { .. }
                    | ClaimSharedResult::Denied(ClaimDeniedReason::CoordinationDisabled)
            ),
            "populated executor must not be Unsupported; got {after:?}"
        );

        cancel.cancel();
    }
}
