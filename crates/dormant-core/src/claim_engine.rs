//! Pure single-flight state transitions for negotiated shared-display claims.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::claim::{ClaimDeniedReason, ClaimVerdict, release_deadline};
use crate::types::DisplayId;

/// Literal transition anchors emitted by the claim engine.
pub const CLAIM_EVENTS: [&str; 10] = [
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
];

/// Requester-side phase for one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequesterStage {
    /// The request has been created but fan-out has not completed.
    Broadcasting,
    /// Fan-out completed and peer verdicts are being collected.
    AwaitingAck,
    /// An owner accepted; hardware observation is authoritative from here.
    Watching {
        /// UX deadline for the negotiated release.
        deadline: Instant,
    },
}

/// Owner-side phase for one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerStage {
    /// `Accepted` was emitted before any release work.
    AckSent,
    /// Wake, hooks, and input write are being sequenced.
    Releasing,
}

/// Terminal result published after the engine removes a flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// Hardware flip was observed and acquisition completed.
    Success,
    /// A diagnosable requester failure.
    Failed(ClaimFailure),
    /// A requester deadline elapsed.
    TimedOut,
    /// The configured display disappeared during reload.
    Removed,
    /// The owner released the display and entered deferred state.
    Released,
    /// A release hook prevented the write.
    ReleaseAborted,
    /// The input write failed after release hooks ran.
    ReleaseFailed,
}

/// Requester failures that do not permit direct fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimFailure {
    /// A recipient rejected both the original and refreshed epoch.
    StaleEpoch,
    /// A peer explicitly denied the claim.
    Denied(ClaimDeniedReason),
    /// The owner reported that release failed.
    ReleaseFailed(String),
}

/// Pure interpretation of the daemon hook runner's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookResult {
    /// The hook slot permits the sequence to continue.
    Completed,
    /// An `abort_on_failure` entry stopped the slot.
    Aborted,
}

/// Input-source capability used during inbound validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimCapability {
    /// The controller can perform input-source writes.
    Writable,
    /// The display is shared for observation only.
    ObserveOnly,
}

/// Owner and reload disposition captured for inbound validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDisposition {
    /// This instance owns the display and can accept work.
    Ready {
        /// The release sequence must wake the panel first.
        standby: bool,
    },
    /// The hardware poll says another input owns the panel.
    NotOwner,
    /// A generation swap is quiescing new claims.
    Quiescing,
}

/// Requester events accepted by [`ClaimEngine::requester_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequesterEvent {
    /// Concurrent peer fan-out was submitted.
    FanoutSent,
    /// One correlated peer verdict arrived.
    Response {
        /// Request correlation nonce copied by the peer.
        nonce: String,
        /// Owner-disposition peer instance id. Required
        /// for per-peer dedup (F1 — a single paired peer
        /// must not be able to force a fallback by emitting
        /// multiple signed `NotOwner` frames, each passing
        /// T8 replay via its own counter).
        peer_instance_id: String,
        /// Owner disposition.
        verdict: ClaimVerdict,
    },
    /// The request/ack deadline elapsed.
    ClaimTimeout,
    /// Polling observed the requester's configured input code.
    FlipObserved,
    /// The acquire hook and wake sequence completed.
    AcquireCompleted,
    /// An accepted owner pushed a release failure.
    ReleaseFailed {
        /// Request correlation nonce copied by the owner.
        nonce: String,
        /// Operator-visible release error.
        reason: String,
    },
    /// Reload removed the target display.
    DisplayRemoved,
}

impl RequesterEvent {
    /// Build a correlated response event.
    #[must_use]
    pub fn response(
        nonce: impl Into<String>,
        peer_instance_id: impl Into<String>,
        verdict: ClaimVerdict,
    ) -> Self {
        Self::Response {
            nonce: nonce.into(),
            peer_instance_id: peer_instance_id.into(),
            verdict,
        }
    }

    /// Build a correlated owner release-failure event.
    #[must_use]
    pub fn release_failed(nonce: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ReleaseFailed {
            nonce: nonce.into(),
            reason: reason.into(),
        }
    }
}

/// Owner events accepted by [`ClaimEngine::owner_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerEvent {
    /// The `Accepted` reply was delivered.
    AckDelivered,
    /// A standby wake attempt completed successfully.
    WakeCompleted,
    /// The blocking `before_release` slot completed or aborted.
    BeforeRelease(HookResult),
    /// The input-source write succeeded.
    WriteSucceeded,
    /// The input-source write failed after `before_release` ran.
    WriteFailed(String),
    /// The normal `after_release` slot completed.
    AfterReleaseCompleted,
    /// A requester sent a best-effort abort.
    AbortReceived {
        /// Request correlation nonce copied by the requester.
        nonce: String,
    },
    /// Reload removed the target display.
    DisplayRemoved,
}

impl OwnerEvent {
    /// Build a correlated pre-release abort event.
    #[must_use]
    pub fn abort(nonce: impl Into<String>) -> Self {
        Self::AbortReceived {
            nonce: nonce.into(),
        }
    }
}

/// Owner-side facts captured atomically when an inbound claim is validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRequest {
    /// Local display identifier.
    pub display: DisplayId,
    /// Identity supplied by the requester.
    pub requested_identity: String,
    /// Locally derived claim identity, if available.
    pub local_identity: Option<String>,
    /// Input code requested by the peer.
    ///
    /// Carried as `u16` over the wire (the `ClaimRequest`
    /// struct) but the OWNER side writes a `u8` (VCP 0x60
    /// is an 8-bit field). A code that exceeds `u8` is
    /// invalid input; `begin_owner` rejects it with
    /// [`ClaimDeniedReason::Unsupported`] rather than
    /// silently truncating to 0 (which would map to the
    /// `MAGIC_STANDBY` sentinel and trigger a spurious F4
    /// standby failure on the requester).
    pub requester_input_code: u16,
    /// This owner's configured input code.
    pub local_input_code: u16,
    /// Input-source write capability.
    pub capability: ClaimCapability,
    /// Ownership, standby, and reload disposition.
    pub disposition: OwnerDisposition,
    /// Sum of blocking hook timeouts plus standby wake budget.
    pub eta: Duration,
}

/// Side effects for the daemon integration layer to execute in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Emit one exact grep-stable lifecycle anchor.
    Trace(&'static str),
    /// Fan a request to all paired peers.
    BroadcastRequest,
    /// Retry a busy request exactly once.
    RetryRequest,
    /// Retry a stale-epoch request with the recipient's current epoch.
    RetryWithEpoch(String),
    /// Begin watching the local input-source poll.
    WatchForFlip,
    /// Send a best-effort abort for a request not known to have released.
    SendAbort,
    /// Perform the fresh-read guarded direct fallback decision.
    AttemptFallback,
    /// Report a concurrent local trigger without replacing the flight.
    BusyLocal,
    /// Send an owner verdict.
    SendVerdict(ClaimVerdict),
    /// Wake a standby panel before release hooks.
    WakeDisplay,
    /// Run `before_release` hooks.
    RunBeforeRelease,
    /// Write the requester's input code.
    WriteInput,
    /// Run `after_release`, with compensation indicated on write failure.
    RunAfterRelease {
        /// Sets `DORMANT_ABORTED=1` when true.
        aborted: bool,
    },
    /// Notify the requester that release did not complete.
    SendReleaseFailed,
    /// Enter owner-deferred state after release completion.
    EnterDeferred,
    /// Run the requester acquire sequence after a confirmed flip.
    RunBeforeAcquire,
    /// Publish a terminal result after cleanup.
    Terminal(Terminal),
}

#[derive(Debug, Clone)]
struct RequesterFlight {
    nonce: String,
    stage: RequesterStage,
    request_deadline: Instant,
    /// Peer instance ids that have already responded to this
    /// flight's `BroadcastRequest`. Per-peer dedup (rather
    /// than a bare counter) prevents a single paired peer
    /// from emitting multiple `NotOwner` frames — each
    /// passing T8 replay via its own signed counter — to
    /// drive the count to zero and force a direct-write
    /// fallback that skips the legitimate owner's hooks.
    responded_peers: std::collections::HashSet<String>,
    /// `responded_peers.len() == expected_peers` is the
    /// terminal condition for the fan-in. Snapshot at
    /// flight-arm time so a late peer addition doesn't
    /// extend the wait indefinitely.
    expected_peers: usize,
    busy_retried: bool,
    epoch_retried: bool,
    flip_observed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerProgress {
    Ack,
    Wake,
    BeforeRelease,
    Write,
    AfterRelease,
}

#[derive(Debug, Clone)]
struct OwnerFlight {
    nonce: String,
    stage: OwnerStage,
    progress: OwnerProgress,
    standby: bool,
    deadline: Instant,
}

#[derive(Debug, Clone)]
enum Flight {
    Requester(RequesterFlight),
    Owner(OwnerFlight),
}

/// Per-display single-flight coordinator with no clock, network, hook, or DDC I/O.
#[derive(Debug)]
pub struct ClaimEngine {
    flights: HashMap<DisplayId, Flight>,
    poll_interval: Duration,
    release_deadline_cap: Duration,
}

impl Default for ClaimEngine {
    fn default() -> Self {
        Self::with_release_policy(Duration::from_secs(2), Duration::from_secs(45))
    }
}

impl ClaimEngine {
    /// Create an engine using configured poll and release-cap values.
    #[must_use]
    pub fn with_release_policy(poll_interval: Duration, release_deadline_cap: Duration) -> Self {
        Self {
            flights: HashMap::new(),
            poll_interval,
            release_deadline_cap,
        }
    }

    /// Start a requester flight or return a local busy decision.
    pub fn begin_requester(
        &mut self,
        display: DisplayId,
        nonce: &str,
        peer_count: usize,
        now: Instant,
        claim_timeout: Duration,
    ) -> Vec<Action> {
        if self.flights.contains_key(&display) {
            return vec![Action::Trace("claim_busy"), Action::BusyLocal];
        }
        self.flights.insert(
            display,
            Flight::Requester(RequesterFlight {
                nonce: nonce.to_owned(),
                stage: RequesterStage::Broadcasting,
                request_deadline: now + claim_timeout,
                responded_peers: std::collections::HashSet::new(),
                expected_peers: peer_count,
                busy_retried: false,
                epoch_retried: false,
                flip_observed: false,
            }),
        );
        vec![Action::Trace("claim_requested"), Action::BroadcastRequest]
    }

    /// Start or reject an owner flight from a validated inbound request.
    pub fn begin_owner(
        &mut self,
        request: OwnerRequest,
        nonce: &str,
        now: Instant,
        release_cap: Duration,
    ) -> Vec<Action> {
        if matches!(request.disposition, OwnerDisposition::Quiescing)
            || self.flights.contains_key(&request.display)
        {
            return vec![
                Action::Trace("claim_busy"),
                Action::SendVerdict(ClaimVerdict::Busy),
            ];
        }
        if request.capability == ClaimCapability::ObserveOnly {
            return denied(ClaimDeniedReason::Unsupported);
        }
        let Some(local_identity) = &request.local_identity else {
            return denied(ClaimDeniedReason::IdentityUnavailable);
        };
        if local_identity != &request.requested_identity
            || matches!(request.disposition, OwnerDisposition::NotOwner)
        {
            return vec![
                Action::Trace("claim_not_owner"),
                Action::SendVerdict(ClaimVerdict::NotOwner),
            ];
        }
        // N2: validate the requester's input code fits the
        // 8-bit VCP 0x60 field the OWNER will write. A
        // value > 255 is invalid input — refusing it here
        // keeps the OWNER's `u8` write from silently
        // truncating to 0 (the standby sentinel) and
        // surfacing a spurious F4 failure on the
        // requester.
        if request.requester_input_code > u16::from(u8::MAX) {
            return denied(ClaimDeniedReason::Unsupported);
        }
        if request.requester_input_code == request.local_input_code {
            return denied(ClaimDeniedReason::InputCodeConflict);
        }
        let eta_ms = millis_u64(request.eta);
        self.flights.insert(
            request.display,
            Flight::Owner(OwnerFlight {
                nonce: nonce.to_owned(),
                stage: OwnerStage::AckSent,
                progress: OwnerProgress::Ack,
                standby: matches!(
                    request.disposition,
                    OwnerDisposition::Ready { standby: true }
                ),
                deadline: now + release_cap,
            }),
        );
        vec![
            Action::Trace("claim_accepted"),
            Action::SendVerdict(ClaimVerdict::Accepted { eta_ms }),
        ]
    }

    /// Apply one requester event; invalid side/state combinations are no-ops.
    pub fn requester_event(
        &mut self,
        display: &DisplayId,
        event: RequesterEvent,
        now: Instant,
    ) -> Vec<Action> {
        let Some(Flight::Requester(flight)) = self.flights.get_mut(display) else {
            return Vec::new();
        };
        let mut terminal = None;
        let actions = requester_transition(
            flight,
            event,
            now,
            self.poll_interval,
            self.release_deadline_cap,
            &mut terminal,
        );
        if terminal.is_some() {
            self.flights.remove(display);
        }
        actions
    }

    /// Apply one owner event; invalid side/state combinations are no-ops.
    pub fn owner_event(&mut self, display: &DisplayId, event: OwnerEvent) -> Vec<Action> {
        let Some(Flight::Owner(flight)) = self.flights.get_mut(display) else {
            return Vec::new();
        };
        let (actions, terminal) = owner_transition(flight, event);
        if terminal {
            self.flights.remove(display);
        }
        actions
    }

    /// Return the requester phase for `display`.
    #[must_use]
    pub fn requester_stage(&self, display: &DisplayId) -> Option<RequesterStage> {
        match self.flights.get(display) {
            Some(Flight::Requester(flight)) => Some(flight.stage),
            _ => None,
        }
    }

    /// Return the owner phase for `display`.
    #[must_use]
    pub fn owner_stage(&self, display: &DisplayId) -> Option<OwnerStage> {
        match self.flights.get(display) {
            Some(Flight::Owner(flight)) => Some(flight.stage),
            _ => None,
        }
    }

    /// Whether ownership-loss handling is suppressed by a live local flight.
    #[must_use]
    pub fn is_suppressed(&self, display: &DisplayId, now: Instant) -> bool {
        self.flights
            .get(display)
            .is_some_and(|flight| match flight {
                Flight::Requester(flight) => match flight.stage {
                    RequesterStage::Watching { deadline } => now < deadline,
                    RequesterStage::Broadcasting | RequesterStage::AwaitingAck => {
                        now < flight.request_deadline
                    }
                },
                Flight::Owner(flight) => now < flight.deadline,
            })
    }

    /// Apply the active phase deadline for one display when it has elapsed.
    pub fn on_deadline(&mut self, display: &DisplayId, now: Instant) -> Vec<Action> {
        let Some(flight) = self.flights.get(display) else {
            return Vec::new();
        };
        let (deadline, role) = match flight {
            Flight::Requester(flight) => match flight.stage {
                RequesterStage::Watching { deadline } => {
                    (deadline, DeadlineRole::RequesterAccepted)
                }
                RequesterStage::Broadcasting | RequesterStage::AwaitingAck => {
                    (flight.request_deadline, DeadlineRole::RequesterUnaccepted)
                }
            },
            Flight::Owner(flight) => (flight.deadline, DeadlineRole::Owner),
        };
        if now < deadline {
            return Vec::new();
        }
        self.flights.remove(display);
        match role {
            DeadlineRole::RequesterUnaccepted => vec![
                Action::SendAbort,
                Action::Trace("claim_fallback_direct"),
                Action::AttemptFallback,
            ],
            DeadlineRole::RequesterAccepted => vec![
                Action::SendAbort,
                Action::Trace("claim_failed"),
                Action::Terminal(Terminal::TimedOut),
            ],
            DeadlineRole::Owner => vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::TimedOut),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlineRole {
    RequesterUnaccepted,
    RequesterAccepted,
    Owner,
}

fn requester_transition(
    flight: &mut RequesterFlight,
    event: RequesterEvent,
    now: Instant,
    poll_interval: Duration,
    release_cap: Duration,
    terminal: &mut Option<Terminal>,
) -> Vec<Action> {
    match (flight.stage, event) {
        (RequesterStage::Broadcasting, RequesterEvent::FanoutSent) => {
            flight.stage = RequesterStage::AwaitingAck;
            Vec::new()
        }
        (
            RequesterStage::AwaitingAck,
            RequesterEvent::Response {
                nonce,
                peer_instance_id,
                verdict,
            },
        ) if nonce == flight.nonce => requester_response(
            flight,
            &peer_instance_id,
            verdict,
            now,
            poll_interval,
            release_cap,
            terminal,
        ),
        (
            RequesterStage::Broadcasting | RequesterStage::AwaitingAck,
            RequesterEvent::ClaimTimeout,
        ) => {
            *terminal = Some(Terminal::TimedOut);
            vec![
                Action::SendAbort,
                Action::Trace("claim_fallback_direct"),
                Action::AttemptFallback,
            ]
        }
        (RequesterStage::Watching { .. }, RequesterEvent::FlipObserved) => {
            flight.flip_observed = true;
            vec![Action::RunBeforeAcquire]
        }
        (RequesterStage::Watching { .. }, RequesterEvent::AcquireCompleted)
            if flight.flip_observed =>
        {
            *terminal = Some(Terminal::Success);
            terminal_actions("claim_completed", Terminal::Success)
        }
        (RequesterStage::Watching { .. }, RequesterEvent::ReleaseFailed { nonce, reason })
            if nonce == flight.nonce =>
        {
            let result = Terminal::Failed(ClaimFailure::ReleaseFailed(reason));
            *terminal = Some(result.clone());
            terminal_actions("claim_failed", result)
        }
        (_, RequesterEvent::DisplayRemoved) => {
            *terminal = Some(Terminal::Removed);
            vec![
                Action::SendAbort,
                Action::Trace("claim_failed"),
                Action::Terminal(Terminal::Removed),
            ]
        }
        _ => Vec::new(),
    }
}

fn requester_response(
    flight: &mut RequesterFlight,
    peer_instance_id: &str,
    verdict: ClaimVerdict,
    now: Instant,
    poll_interval: Duration,
    release_cap: Duration,
    terminal: &mut Option<Terminal>,
) -> Vec<Action> {
    match verdict {
        ClaimVerdict::Accepted { eta_ms } => {
            let bound = release_deadline(Duration::from_millis(eta_ms), poll_interval, release_cap);
            flight.stage = RequesterStage::Watching {
                deadline: now + bound,
            };
            vec![Action::Trace("claim_accepted"), Action::WatchForFlip]
        }
        ClaimVerdict::NotOwner => {
            // F1: per-peer dedup. A single paired peer that
            // emits multiple signed `NotOwner` frames (each
            // with its own counter, each passing T8 replay)
            // must NOT be able to force a fallback that skips
            // the legitimate owner's hooks. Only the first
            // `NotOwner` from each distinct peer counts; the
            // fan-in completes when every expected peer has
            // spoken.
            if !flight.responded_peers.insert(peer_instance_id.to_owned()) {
                return Vec::new();
            }
            if flight.responded_peers.len() >= flight.expected_peers {
                *terminal = Some(Terminal::TimedOut);
                vec![
                    Action::Trace("claim_not_owner"),
                    Action::Trace("claim_fallback_direct"),
                    Action::AttemptFallback,
                ]
            } else {
                vec![Action::Trace("claim_not_owner")]
            }
        }
        ClaimVerdict::Busy => {
            if flight.busy_retried {
                vec![Action::Trace("claim_busy")]
            } else {
                flight.busy_retried = true;
                vec![Action::Trace("claim_busy"), Action::RetryRequest]
            }
        }
        ClaimVerdict::Denied(ClaimDeniedReason::StaleEpoch { recipient_epoch }) => {
            if flight.epoch_retried {
                let result = Terminal::Failed(ClaimFailure::StaleEpoch);
                *terminal = Some(result.clone());
                vec![
                    Action::Trace("claim_denied"),
                    Action::Trace("claim_failed"),
                    Action::Terminal(result),
                ]
            } else {
                flight.epoch_retried = true;
                vec![Action::RetryWithEpoch(recipient_epoch)]
            }
        }
        ClaimVerdict::Denied(reason) => {
            let result = Terminal::Failed(ClaimFailure::Denied(reason));
            *terminal = Some(result.clone());
            vec![
                Action::Trace("claim_denied"),
                Action::Trace("claim_failed"),
                Action::Terminal(result),
            ]
        }
    }
}

fn owner_transition(flight: &mut OwnerFlight, event: OwnerEvent) -> (Vec<Action>, bool) {
    match (flight.progress, event) {
        (OwnerProgress::Ack, OwnerEvent::AckDelivered) => {
            flight.stage = OwnerStage::Releasing;
            if flight.standby {
                flight.progress = OwnerProgress::Wake;
                (vec![Action::WakeDisplay], false)
            } else {
                flight.progress = OwnerProgress::BeforeRelease;
                (vec![Action::RunBeforeRelease], false)
            }
        }
        (OwnerProgress::Wake, OwnerEvent::WakeCompleted) => {
            flight.progress = OwnerProgress::BeforeRelease;
            (vec![Action::RunBeforeRelease], false)
        }
        (OwnerProgress::BeforeRelease, OwnerEvent::BeforeRelease(HookResult::Completed)) => {
            flight.progress = OwnerProgress::Write;
            (vec![Action::WriteInput], false)
        }
        (OwnerProgress::BeforeRelease, OwnerEvent::BeforeRelease(HookResult::Aborted)) => (
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::ReleaseAborted),
            ],
            true,
        ),
        (OwnerProgress::Ack, OwnerEvent::AbortReceived { nonce }) if nonce == flight.nonce => (
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::ReleaseAborted),
            ],
            true,
        ),
        (OwnerProgress::Write, OwnerEvent::WriteSucceeded) => {
            flight.progress = OwnerProgress::AfterRelease;
            (vec![Action::RunAfterRelease { aborted: false }], false)
        }
        (OwnerProgress::Write, OwnerEvent::WriteFailed(_)) => (
            vec![
                Action::Trace("claim_release_failed"),
                Action::SendReleaseFailed,
                Action::RunAfterRelease { aborted: true },
                Action::Terminal(Terminal::ReleaseFailed),
            ],
            true,
        ),
        (OwnerProgress::AfterRelease, OwnerEvent::AfterReleaseCompleted) => (
            vec![Action::EnterDeferred, Action::Terminal(Terminal::Released)],
            true,
        ),
        (_, OwnerEvent::DisplayRemoved) => (
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::Removed),
            ],
            true,
        ),
        _ => (Vec::new(), false),
    }
}

fn denied(reason: ClaimDeniedReason) -> Vec<Action> {
    vec![
        Action::Trace("claim_denied"),
        Action::SendVerdict(ClaimVerdict::Denied(reason)),
    ]
}

fn terminal_actions(event: &'static str, terminal: Terminal) -> Vec<Action> {
    vec![Action::Trace(event), Action::Terminal(terminal)]
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        Action, ClaimCapability, ClaimEngine, ClaimFailure, HookResult, OwnerDisposition,
        OwnerEvent, OwnerRequest, OwnerStage, RequesterEvent, RequesterStage, Terminal,
    };
    use crate::claim::{ClaimDeniedReason, ClaimVerdict};
    use crate::types::DisplayId;

    fn display() -> DisplayId {
        DisplayId("monitor".into())
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn owner_request() -> OwnerRequest {
        OwnerRequest {
            display: display(),
            requested_identity: "aoc:agon:serial".into(),
            local_identity: Some("aoc:agon:serial".into()),
            requester_input_code: 0x0f,
            local_input_code: 0x11,
            capability: ClaimCapability::Writable,
            disposition: OwnerDisposition::Ready { standby: false },
            eta: Duration::from_secs(5),
        }
    }

    #[test]
    fn requester_happy_path_is_suppressed_until_confirmed_acquire() {
        let start = now();
        let mut engine = ClaimEngine::default();
        assert_eq!(
            engine.begin_requester(display(), "nonce", 2, start, Duration::from_secs(3)),
            vec![Action::Trace("claim_requested"), Action::BroadcastRequest]
        );
        assert!(engine.is_suppressed(&display(), start));
        assert_eq!(
            engine.requester_stage(&display()),
            Some(RequesterStage::Broadcasting)
        );
        assert!(
            engine
                .requester_event(&display(), RequesterEvent::FanoutSent, start)
                .is_empty()
        );
        assert_eq!(
            engine.requester_stage(&display()),
            Some(RequesterStage::AwaitingAck)
        );

        let actions = engine.requester_event(
            &display(),
            RequesterEvent::response("nonce", "peer", ClaimVerdict::Accepted { eta_ms: 5_000 }),
            start,
        );
        assert_eq!(
            actions,
            vec![Action::Trace("claim_accepted"), Action::WatchForFlip]
        );
        assert!(matches!(
            engine.requester_stage(&display()),
            Some(RequesterStage::Watching { .. })
        ));
        assert_eq!(
            engine.requester_event(&display(), RequesterEvent::FlipObserved, start),
            vec![Action::RunBeforeAcquire]
        );
        assert_eq!(
            engine.requester_event(&display(), RequesterEvent::AcquireCompleted, start),
            vec![
                Action::Trace("claim_completed"),
                Action::Terminal(Terminal::Success)
            ]
        );
        assert!(!engine.is_suppressed(&display(), start));
    }

    #[test]
    fn requester_fan_in_retry_and_failure_paths_are_bounded() {
        let start = now();
        let mut engine = ClaimEngine::default();
        engine.begin_requester(display(), "nonce", 2, start, Duration::from_secs(3));
        engine.requester_event(&display(), RequesterEvent::FanoutSent, start);
        assert!(
            engine
                .requester_event(
                    &display(),
                    RequesterEvent::response("nonce", "peer-a", ClaimVerdict::NotOwner),
                    start,
                )
                .contains(&Action::Trace("claim_not_owner"))
        );
        assert_eq!(
            engine.requester_event(
                &display(),
                RequesterEvent::response("nonce", "peer-b", ClaimVerdict::NotOwner),
                start,
            ),
            vec![
                Action::Trace("claim_not_owner"),
                Action::Trace("claim_fallback_direct"),
                Action::AttemptFallback
            ]
        );
        assert!(!engine.is_suppressed(&display(), start));

        engine.begin_requester(display(), "busy", 1, start, Duration::from_secs(3));
        engine.requester_event(&display(), RequesterEvent::FanoutSent, start);
        assert_eq!(
            engine.requester_event(
                &display(),
                RequesterEvent::response("busy", "peer-c", ClaimVerdict::Busy),
                start
            ),
            vec![Action::Trace("claim_busy"), Action::RetryRequest]
        );
        assert_eq!(
            engine.requester_event(
                &display(),
                RequesterEvent::response("busy", "peer-c", ClaimVerdict::Busy),
                start
            ),
            vec![Action::Trace("claim_busy")]
        );
        assert_eq!(
            engine.requester_event(&display(), RequesterEvent::ClaimTimeout, start),
            vec![
                Action::SendAbort,
                Action::Trace("claim_fallback_direct"),
                Action::AttemptFallback
            ]
        );
    }

    #[test]
    fn stale_epoch_retries_once_and_denial_fails_visibly() {
        let start = now();
        let mut engine = ClaimEngine::default();
        engine.begin_requester(display(), "nonce", 1, start, Duration::from_secs(3));
        engine.requester_event(&display(), RequesterEvent::FanoutSent, start);
        let stale = ClaimVerdict::Denied(ClaimDeniedReason::StaleEpoch {
            recipient_epoch: "fresh-epoch-0001".into(),
        });
        assert_eq!(
            engine.requester_event(
                &display(),
                RequesterEvent::response("nonce", "peer", stale.clone()),
                start,
            ),
            vec![Action::RetryWithEpoch("fresh-epoch-0001".into())]
        );
        assert_eq!(
            engine.requester_event(
                &display(),
                RequesterEvent::response("nonce", "peer", stale),
                start,
            ),
            vec![
                Action::Trace("claim_denied"),
                Action::Trace("claim_failed"),
                Action::Terminal(Terminal::Failed(ClaimFailure::StaleEpoch))
            ]
        );
        assert!(!engine.is_suppressed(&display(), start));
    }

    #[test]
    fn owner_validates_then_sequences_ack_release_and_deferred_entry() {
        let start = now();
        let mut engine = ClaimEngine::default();
        assert_eq!(
            engine.begin_owner(owner_request(), "nonce", start, Duration::from_secs(45)),
            vec![
                Action::Trace("claim_accepted"),
                Action::SendVerdict(ClaimVerdict::Accepted { eta_ms: 5_000 })
            ]
        );
        assert_eq!(engine.owner_stage(&display()), Some(OwnerStage::AckSent));
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::AckDelivered),
            vec![Action::RunBeforeRelease]
        );
        assert_eq!(engine.owner_stage(&display()), Some(OwnerStage::Releasing));
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::BeforeRelease(HookResult::Completed)),
            vec![Action::WriteInput]
        );
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::WriteSucceeded),
            vec![Action::RunAfterRelease { aborted: false }]
        );
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::AfterReleaseCompleted),
            vec![Action::EnterDeferred, Action::Terminal(Terminal::Released)]
        );
        assert!(!engine.is_suppressed(&display(), start));
    }

    #[test]
    fn owner_abort_write_failure_abort_message_and_removal_compensate() {
        let start = now();
        let mut engine = ClaimEngine::default();
        engine.begin_owner(owner_request(), "abort", start, Duration::from_secs(45));
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::abort("abort")),
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::ReleaseAborted)
            ]
        );

        engine.begin_owner(owner_request(), "hook", start, Duration::from_secs(45));
        engine.owner_event(&display(), OwnerEvent::AckDelivered);
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::BeforeRelease(HookResult::Aborted)),
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::ReleaseAborted)
            ]
        );

        engine.begin_owner(owner_request(), "write", start, Duration::from_secs(45));
        engine.owner_event(&display(), OwnerEvent::AckDelivered);
        engine.owner_event(&display(), OwnerEvent::BeforeRelease(HookResult::Completed));
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::WriteFailed("ddc failed".into())),
            vec![
                Action::Trace("claim_release_failed"),
                Action::SendReleaseFailed,
                Action::RunAfterRelease { aborted: true },
                Action::Terminal(Terminal::ReleaseFailed)
            ]
        );

        engine.begin_owner(owner_request(), "removed", start, Duration::from_secs(45));
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::DisplayRemoved),
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::Removed)
            ]
        );
    }

    #[test]
    fn validation_single_flight_deadlines_and_illegal_events_are_explicit() {
        let start = now();
        let mut engine = ClaimEngine::default();
        let mut request = owner_request();
        request.disposition = OwnerDisposition::NotOwner;
        assert_eq!(
            engine.begin_owner(request, "n", start, Duration::from_secs(45)),
            vec![
                Action::Trace("claim_not_owner"),
                Action::SendVerdict(ClaimVerdict::NotOwner)
            ]
        );
        let mut request = owner_request();
        request.capability = ClaimCapability::ObserveOnly;
        assert!(
            engine
                .begin_owner(request, "n", start, Duration::from_secs(45))
                .contains(&Action::Trace("claim_denied"))
        );
        let mut request = owner_request();
        request.local_identity = None;
        assert!(
            engine
                .begin_owner(request, "n", start, Duration::from_secs(45))
                .contains(&Action::SendVerdict(ClaimVerdict::Denied(
                    ClaimDeniedReason::IdentityUnavailable
                )))
        );
        let mut request = owner_request();
        request.requester_input_code = request.local_input_code;
        assert!(
            engine
                .begin_owner(request, "n", start, Duration::from_secs(45))
                .contains(&Action::SendVerdict(ClaimVerdict::Denied(
                    ClaimDeniedReason::InputCodeConflict
                )))
        );

        engine.begin_requester(display(), "local", 1, start, Duration::from_secs(3));
        assert_eq!(
            engine.begin_requester(display(), "second", 1, start, Duration::from_secs(3)),
            vec![Action::Trace("claim_busy"), Action::BusyLocal]
        );
        assert!(
            engine
                .owner_event(&display(), OwnerEvent::WriteSucceeded)
                .is_empty()
        );
        assert_eq!(
            engine.requester_event(&display(), RequesterEvent::DisplayRemoved, start),
            vec![
                Action::SendAbort,
                Action::Trace("claim_failed"),
                Action::Terminal(Terminal::Removed)
            ]
        );

        engine.begin_requester(display(), "expiry", 1, start, Duration::from_secs(3));
        assert_eq!(
            engine.on_deadline(&display(), start + Duration::from_secs(3)),
            vec![
                Action::SendAbort,
                Action::Trace("claim_fallback_direct"),
                Action::AttemptFallback,
            ]
        );
        assert!(!engine.is_suppressed(&display(), start + Duration::from_secs(3)));
    }

    #[test]
    fn stale_correlations_and_mid_release_abort_are_no_ops() {
        let start = now();
        let mut engine = ClaimEngine::default();
        engine.begin_requester(display(), "current", 1, start, Duration::from_secs(3));
        engine.requester_event(&display(), RequesterEvent::FanoutSent, start);
        assert!(
            engine
                .requester_event(
                    &display(),
                    RequesterEvent::response(
                        "stale",
                        "peer",
                        ClaimVerdict::Accepted { eta_ms: 1_000 },
                    ),
                    start,
                )
                .is_empty()
        );
        assert_eq!(
            engine.requester_stage(&display()),
            Some(RequesterStage::AwaitingAck)
        );

        engine.requester_event(&display(), RequesterEvent::DisplayRemoved, start);
        engine.begin_owner(owner_request(), "owner", start, Duration::from_secs(45));
        assert!(
            engine
                .owner_event(&display(), OwnerEvent::abort("stale"))
                .is_empty()
        );
        engine.owner_event(&display(), OwnerEvent::AckDelivered);
        assert!(
            engine
                .owner_event(&display(), OwnerEvent::abort("owner"))
                .is_empty()
        );
        assert_eq!(engine.owner_stage(&display()), Some(OwnerStage::Releasing));
    }

    #[test]
    fn deadlines_are_inclusive_and_lift_suppression_for_both_roles() {
        let start = now();
        let mut engine = ClaimEngine::default();
        engine.begin_requester(display(), "watch", 1, start, Duration::from_secs(3));
        engine.requester_event(&display(), RequesterEvent::FanoutSent, start);
        engine.requester_event(
            &display(),
            RequesterEvent::response("watch", "peer", ClaimVerdict::Accepted { eta_ms: 5_000 }),
            start,
        );
        let deadline = start + Duration::from_secs(5);
        let before_deadline = deadline.checked_sub(Duration::from_nanos(1)).unwrap();
        assert!(engine.is_suppressed(&display(), before_deadline));
        assert!(!engine.is_suppressed(&display(), deadline));
        assert!(engine.on_deadline(&display(), before_deadline).is_empty());
        assert_eq!(
            engine.on_deadline(&display(), deadline),
            vec![
                Action::SendAbort,
                Action::Trace("claim_failed"),
                Action::Terminal(Terminal::TimedOut),
            ]
        );
        assert!(engine.requester_stage(&display()).is_none());

        engine.begin_owner(owner_request(), "owner", start, Duration::from_secs(45));
        assert_eq!(
            engine.on_deadline(&display(), start + Duration::from_secs(45)),
            vec![
                Action::Trace("claim_release_aborted"),
                Action::SendReleaseFailed,
                Action::Terminal(Terminal::TimedOut),
            ]
        );
        assert!(engine.owner_stage(&display()).is_none());
    }

    #[test]
    fn standby_owner_wakes_before_hooks_and_reload_quiesce_is_busy() {
        let start = now();
        let mut engine = ClaimEngine::default();
        let mut request = owner_request();
        request.disposition = OwnerDisposition::Quiescing;
        assert_eq!(
            engine.begin_owner(request, "q", start, Duration::from_secs(45)),
            vec![
                Action::Trace("claim_busy"),
                Action::SendVerdict(ClaimVerdict::Busy),
            ]
        );

        let mut request = owner_request();
        request.disposition = OwnerDisposition::Ready { standby: true };
        engine.begin_owner(request, "standby", start, Duration::from_secs(45));
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::AckDelivered),
            vec![Action::WakeDisplay]
        );
        assert_eq!(
            engine.owner_event(&display(), OwnerEvent::WakeCompleted),
            vec![Action::RunBeforeRelease]
        );
    }

    #[test]
    fn opposing_claims_converge_only_from_each_local_hardware_observation() {
        let start = now();
        let mut left = ClaimEngine::default();
        let mut right = ClaimEngine::default();
        for (engine, nonce) in [(&mut left, "left"), (&mut right, "right")] {
            engine.begin_requester(display(), nonce, 1, start, Duration::from_secs(3));
            engine.requester_event(&display(), RequesterEvent::FanoutSent, start);
            engine.requester_event(
                &display(),
                RequesterEvent::response(nonce, nonce, ClaimVerdict::Accepted { eta_ms: 5_000 }),
                start,
            );
        }
        assert_eq!(
            left.requester_event(&display(), RequesterEvent::FlipObserved, start),
            vec![Action::RunBeforeAcquire]
        );
        assert!(
            right
                .on_deadline(&display(), start + Duration::from_secs(5))
                .contains(&Action::Trace("claim_failed"))
        );
        assert_eq!(
            left.requester_event(&display(), RequesterEvent::AcquireCompleted, start),
            vec![
                Action::Trace("claim_completed"),
                Action::Terminal(Terminal::Success),
            ]
        );
    }

    #[test]
    fn event_vocabulary_is_exact_and_complete() {
        assert_eq!(
            super::CLAIM_EVENTS,
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
            ]
        );
    }

    #[test]
    fn requester_state_event_matrix_marks_every_legal_and_illegal_pair() {
        let start = now();
        let stages = [
            RequesterStage::Broadcasting,
            RequesterStage::AwaitingAck,
            RequesterStage::Watching {
                deadline: start + Duration::from_secs(5),
            },
        ];
        for stage in stages {
            for event_index in 0..7 {
                let mut engine = ClaimEngine::default();
                engine.flights.insert(
                    display(),
                    super::Flight::Requester(super::RequesterFlight {
                        nonce: "n".into(),
                        stage,
                        request_deadline: start + Duration::from_secs(3),
                        responded_peers: std::collections::HashSet::new(),
                        expected_peers: 1,
                        busy_retried: false,
                        epoch_retried: false,
                        flip_observed: event_index == 4,
                    }),
                );
                let event = match event_index {
                    0 => RequesterEvent::FanoutSent,
                    1 => RequesterEvent::response("n", "n", ClaimVerdict::NotOwner),
                    2 => RequesterEvent::ClaimTimeout,
                    3 => RequesterEvent::FlipObserved,
                    4 => RequesterEvent::AcquireCompleted,
                    5 => RequesterEvent::release_failed("n", "failed"),
                    6 => RequesterEvent::DisplayRemoved,
                    _ => unreachable!(),
                };
                let before = engine.requester_stage(&display());
                let actions = engine.requester_event(&display(), event, start);
                let changed = !actions.is_empty() || engine.requester_stage(&display()) != before;
                let legal = match stage {
                    RequesterStage::Broadcasting => matches!(event_index, 0 | 2 | 6),
                    RequesterStage::AwaitingAck => matches!(event_index, 1 | 2 | 6),
                    RequesterStage::Watching { .. } => matches!(event_index, 3..=6),
                };
                assert_eq!(changed, legal, "stage={stage:?}, event_index={event_index}");
            }
        }
    }

    #[test]
    fn owner_progress_event_matrix_marks_every_legal_and_illegal_pair() {
        let start = now();
        let progress_steps = [
            super::OwnerProgress::Ack,
            super::OwnerProgress::Wake,
            super::OwnerProgress::BeforeRelease,
            super::OwnerProgress::Write,
            super::OwnerProgress::AfterRelease,
        ];
        for progress in progress_steps {
            for event_index in 0..9 {
                let mut engine = ClaimEngine::default();
                engine.flights.insert(
                    display(),
                    super::Flight::Owner(super::OwnerFlight {
                        nonce: "n".into(),
                        stage: if progress == super::OwnerProgress::Ack {
                            OwnerStage::AckSent
                        } else {
                            OwnerStage::Releasing
                        },
                        progress,
                        standby: progress == super::OwnerProgress::Wake,
                        deadline: start + Duration::from_secs(45),
                    }),
                );
                let event = match event_index {
                    0 => OwnerEvent::AckDelivered,
                    1 => OwnerEvent::WakeCompleted,
                    2 => OwnerEvent::BeforeRelease(HookResult::Completed),
                    3 => OwnerEvent::BeforeRelease(HookResult::Aborted),
                    4 => OwnerEvent::WriteSucceeded,
                    5 => OwnerEvent::WriteFailed("failed".into()),
                    6 => OwnerEvent::AfterReleaseCompleted,
                    7 => OwnerEvent::abort("n"),
                    8 => OwnerEvent::DisplayRemoved,
                    _ => unreachable!(),
                };
                let changed = !engine.owner_event(&display(), event).is_empty();
                let legal = match progress {
                    super::OwnerProgress::Ack => matches!(event_index, 0 | 7 | 8),
                    super::OwnerProgress::Wake => matches!(event_index, 1 | 8),
                    super::OwnerProgress::BeforeRelease => matches!(event_index, 2 | 3 | 8),
                    super::OwnerProgress::Write => matches!(event_index, 4 | 5 | 8),
                    super::OwnerProgress::AfterRelease => matches!(event_index, 6 | 8),
                };
                assert_eq!(
                    changed, legal,
                    "progress={progress:?}, event_index={event_index}"
                );
            }
        }
    }
}
