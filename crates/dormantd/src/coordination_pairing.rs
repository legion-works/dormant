//! Confirmed SPAKE2 pairing sessions for dormant instances.

#![allow(
    dead_code,
    reason = "Task 14 supplies the TCP adapter; Task 13 keeps its independently testable frame state machine private until then."
)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use dormant_core::peers::{
    PAIR_PROTOCOL_VERSION, PairError, PairFrame, PairRole, PeerRecord, build_pairing_transcript,
    load_or_create_identity, upsert_peer,
};
use ed25519_dalek::VerifyingKey;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

const PAIRING_CONTEXT: &[u8] = b"dormant-pairing-v2";
const MAX_PAIR_ATTEMPTS: usize = 10;
const NONCE_BYTES: usize = 32;
const MAC_BYTES: usize = 32;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

type HmacSha256 = Hmac<Sha256>;

/// Local failures that are not part of the ratified pairing wire protocol.
#[derive(Debug)]
pub(crate) enum PairSessionError {
    /// A protocol-level failure reported by the peer or detected locally.
    Wire(PairError),
    /// The selected discovery record identifies this daemon.
    SelfPair,
    /// A session is already active for this pairing window.
    Busy,
    /// Pairing is disabled by runtime configuration.
    Disabled,
    /// The requested pairing window no longer exists.
    UnknownPair,
    /// The transport layer has not yet connected a discovered peer.
    TransportUnavailable,
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
            Self::TransportUnavailable => formatter.write_str("pairing transport is unavailable"),
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
    Paired,
    /// The operator cancelled the window.
    Cancelled,
    /// The window elapsed before a successful confirmation.
    Timeout,
    /// A session ended without producing a peer record.
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

/// A freshly opened responder window. The code is held only in daemon memory.
pub(crate) struct OpenPairing {
    /// Public pairing-window identifier.
    pub pair_id: String,
    /// Pairing code for the local operator surface; never returned by daemon IPC.
    pub code: String,
    /// Public window deadline.
    pub expires_at: Instant,
}

struct LocalPeer {
    state_dir: PathBuf,
    display_name: String,
    identity: dormant_core::peers::InstanceIdentity,
}

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
struct PeerIdentity {
    instance_id: String,
    display_name: String,
    public_key: [u8; 32],
    nonce: [u8; NONCE_BYTES],
}

/// A bounded responder pairing window.
pub(crate) struct PairingWindow {
    pair_id: String,
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
    enabled: bool,
    pairing_window: Duration,
    windows: Mutex<HashMap<String, PairingWindow>>,
}

impl PairingManager {
    /// Construct the runtime pairing manager. It opens no listener by itself.
    #[must_use]
    pub(crate) fn new(state_dir: PathBuf, enabled: bool, pairing_window: Duration) -> Self {
        Self {
            state_dir,
            enabled,
            pairing_window,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Open one responder window. Transport advertisement is attached by Task 14.
    pub(crate) fn open(&self, _display_name: String) -> Result<OpenPairing, PairSessionError> {
        if !self.enabled {
            return Err(PairSessionError::Disabled);
        }
        let (window, open) = PairingWindow::open(self.pairing_window)?;
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
        let local = LocalPeer::load(self.state_dir.clone(), String::new())?;
        if local.identity.instance_id == instance_id {
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

impl PairingWindow {
    /// Open a responder window with fresh OS entropy for both its code and ID.
    pub(crate) fn open(pairing_window: Duration) -> Result<(Self, OpenPairing), PairSessionError> {
        let mut code_bytes = [0_u8; 5];
        getrandom::fill(&mut code_bytes)
            .map_err(|error| PairSessionError::Local(format!("generate pairing code: {error}")))?;
        let code = encode_crockford(code_bytes);

        let mut pair_id_bytes = [0_u8; 16];
        getrandom::fill(&mut pair_id_bytes)
            .map_err(|error| PairSessionError::Local(format!("generate pairing id: {error}")))?;
        let pair_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair_id_bytes);
        let expires_at = Instant::now() + pairing_window;
        let open = OpenPairing {
            pair_id: pair_id.clone(),
            code: code.clone(),
            expires_at,
        };
        Ok((
            Self {
                pair_id,
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

struct SessionFlight {
    in_flight: Arc<AtomicBool>,
}

impl Drop for SessionFlight {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

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

struct ResponderState {
    local: PeerIdentity,
    remote: PeerIdentity,
    key: Zeroizing<Vec<u8>>,
    confirmation_seen: bool,
}

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
    let nonce = decode_exact(&nonce, NONCE_BYTES)?;
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
    let remote_nonce = decode_exact(&nonce, NONCE_BYTES)?;
    let msg2 = wire_to_frame(&frames[1])?;
    let PairFrame::Spake2Msg2 { message } = msg2 else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let inbound = decode_base64(&message)?;
    let key = state
        .spake
        .take()
        .expect("SPAKE2 state is consumed exactly once")
        .finish(&inbound)
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))?;
    let identity = wire_to_frame(&frames[2])?;
    let PairFrame::IdentityExchange { ed25519_pub } = identity else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let remote_public_key = decode_exact(&ed25519_pub, 32)?;
    if remote_public_key != expected_public_key
        || instance_id_for_key(&remote_public_key) != state.remote_instance_id
        || VerifyingKey::from_bytes(&remote_public_key).is_err()
    {
        return Err(PairSessionError::Wire(PairError::InstanceIdConflict));
    }
    state.remote_public_key = Some(remote_public_key);
    state.remote_nonce = Some(remote_nonce);
    state.remote_display_name = Some(display_name);
    state.key = Some(Zeroizing::new(key));

    let transcript = transcript_for_initiator(state)?;
    let mac = confirmation(state.key.as_deref().expect("set above"), &transcript);
    Ok(frames_to_wire(&[
        PairFrame::IdentityExchange {
            ed25519_pub: base64::engine::general_purpose::STANDARD.encode(state.local.public_key),
        },
        PairFrame::KeyConfirm {
            mac: base64::engine::general_purpose::STANDARD.encode(mac),
        },
    ]))
}

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
    let key = decode_exact(&ed25519_pub, 32)?;
    if key != state.remote.public_key || instance_id_for_key(&key) != state.remote.instance_id {
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

fn initiator_receive_confirmation(
    state: &InitiatorState,
    frame: &[u8],
) -> Result<Vec<u8>, PairSessionError> {
    let transcript = transcript_for_initiator(state)?;
    verify_confirmation(
        state
            .key
            .as_deref()
            .expect("initiator key set before confirmation"),
        &transcript,
        wire_to_frame(frame)?,
    )?;
    Ok(frame_for_wire(&PairFrame::PairResult {
        accepted: true,
        error: None,
    }))
}

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
        let mut nonce: [u8; NONCE_BYTES] = decode_exact(&nonce, NONCE_BYTES)?;
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
        let mut public_key: [u8; 32] = decode_exact(&ed25519_pub, 32)?;
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

fn confirmation(key: &[u8], transcript: &[u8]) -> [u8; MAC_BYTES] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

fn verify_confirmation(
    key: &[u8],
    transcript: &[u8],
    frame: PairFrame,
) -> Result<(), PairSessionError> {
    let PairFrame::KeyConfirm { mac } = frame else {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    };
    let received: [u8; MAC_BYTES] = decode_exact(&mac, MAC_BYTES)?;
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

fn decode_exact<const N: usize>(
    encoded: &str,
    expected: usize,
) -> Result<[u8; N], PairSessionError> {
    let bytes = decode_base64(encoded)?;
    bytes
        .try_into()
        .map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
        .and_then(|array: [u8; N]| {
            if expected == N {
                Ok(array)
            } else {
                Err(PairSessionError::Wire(PairError::InvalidFrame))
            }
        })
}

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

fn instance_id_for_key(key: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key)
}

fn frame_for_wire(frame: &PairFrame) -> Vec<u8> {
    let payload = serde_json::to_vec(&frame).expect("PairFrame serialization is infallible");
    let length = u32::try_from(payload.len()).expect("PairFrame payload fits u32");
    let mut wire = Vec::with_capacity(payload.len() + 4);
    wire.extend_from_slice(&length.to_be_bytes());
    wire.extend_from_slice(&payload);
    wire
}

fn frames_to_wire(frames: &[PairFrame]) -> Vec<Vec<u8>> {
    frames.iter().map(frame_for_wire).collect()
}

fn wire_to_frame(wire: &[u8]) -> Result<PairFrame, PairSessionError> {
    if wire.len() < 5 {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    let length = u32::from_be_bytes(wire[..4].try_into().expect("exact prefix"));
    let payload = &wire[4..];
    if !(1..=65_536).contains(&length) || usize::try_from(length).ok() != Some(payload.len()) {
        return Err(PairSessionError::Wire(PairError::InvalidFrame));
    }
    serde_json::from_slice(payload).map_err(|_| PairSessionError::Wire(PairError::InvalidFrame))
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

    macro_rules! assert_wire_error {
        ($actual:expr, $expected:pat) => {
            assert!(matches!($actual, Err(PairSessionError::Wire($expected))));
        };
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
}
