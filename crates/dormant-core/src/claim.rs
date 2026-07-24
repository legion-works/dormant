//! Signed peer-claim messages, replay protection, and deadline policy.

use crate::peers::{InstanceIdentity, PeerRecord};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer as _, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, time::Duration};
use thiserror::Error;

/// Validated fixed-width boot epoch used by the claim protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch(String);

impl Epoch {
    /// Return the epoch's canonical wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for Epoch {
    type Error = ClaimFrameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.len() != 16 || value.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(ClaimFrameError::InvalidEpoch);
        }
        Ok(Self(value.to_owned()))
    }
}

/// Claim protocol version bound into every signature.
pub const CLAIM_PROTOCOL_VERSION: u16 = 1;

/// A signed coordination message from one paired instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimFrame {
    /// Instance ID derived from the signing public key.
    pub sender_instance_id: String,
    /// Fresh sender boot epoch, allowing recipients to bind replies to this daemon run.
    pub sender_epoch: String,
    /// Instance ID of the paired daemon that must process this frame.
    pub recipient_instance_id: String,
    /// Recipient boot epoch known by the sender when this frame was created.
    pub recipient_epoch: String,
    /// Strictly increasing sender-local counter.
    pub counter: u64,
    /// Per-frame unique nonce, encoded for the JSON transport.
    pub nonce: String,
    /// Authenticated claim message.
    pub message: ClaimMessage,
    /// Standard-base64 Ed25519 signature over the canonical payload.
    pub signature: String,
}

/// One claim-protocol message carried by a [`ClaimFrame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaimMessage {
    /// Request that the current owner release a shared display.
    ClaimRequest(ClaimRequest),
    /// Reply to a claim request.
    ClaimResponse(ClaimResponse),
    /// Notify a requester that the owner could not release the display.
    ReleaseFailed(ReleaseFailed),
    /// Cancel a request before the owner starts its release sequence.
    ClaimAbort(ClaimAbort),
    /// Ask an owner for its local idle duration.
    IdleQuery(IdleQuery),
    /// Report an owner's local idle duration.
    IdleReport(IdleReport),
}

/// Request to transfer a shared display to a paired instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// Stable identity derived from the shared display's EDID.
    pub display_identity: String,
    /// Instance ID of the requester.
    pub requester_instance_id: String,
    /// Input code the owner must select.
    pub requester_input_code: u16,
    /// Requester's strictly increasing counter.
    pub counter: u64,
    /// Request correlation nonce.
    pub nonce: String,
}

/// Response to a [`ClaimRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimResponse {
    /// Nonce copied from the request being answered.
    pub nonce: String,
    /// Owner's disposition of the request.
    pub verdict: ClaimVerdict,
}

/// Owner disposition of a claim request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum ClaimVerdict {
    /// The owner accepted the claim and estimates the release completion time.
    Accepted {
        /// Estimated release completion in milliseconds.
        eta_ms: u64,
    },
    /// The peer does not own the requested display.
    NotOwner,
    /// The peer refuses the claim for a diagnosable reason.
    Denied(ClaimDeniedReason),
    /// Another claim or reload already occupies the peer.
    Busy,
}

/// Diagnosable reasons an owner can refuse a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimDeniedReason {
    /// The display cannot select inputs.
    Unsupported,
    /// The display identity could not be derived at claim time.
    IdentityUnavailable,
    /// The requester selected the owner's configured input code.
    InputCodeConflict,
    /// The display vanished during coordination.
    DisplayRemoved,
    /// Coordination is disabled locally.
    CoordinationDisabled,
    /// The requester used an epoch from an earlier daemon run.
    StaleEpoch {
        /// The recipient's current epoch for one retry.
        recipient_epoch: String,
    },
    /// A newer peer supplied an unrecognized reason.
    #[serde(other)]
    Unknown,
}

/// Best-effort notification that an accepted release failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFailed {
    /// Nonce copied from the request whose release failed.
    pub nonce: String,
    /// Operator-visible failure reason.
    pub reason: String,
}

/// Best-effort cancellation for a request that has not started release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimAbort {
    /// Nonce of the request to abort.
    pub nonce: String,
}

/// Query for the current owner's idle duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleQuery {
    /// Correlation nonce for the idle query.
    pub nonce: String,
}

/// Reply containing the current owner's idle duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleReport {
    /// Owner-local idle duration in milliseconds.
    pub idle_ms: u64,
    /// Owner's strictly increasing counter.
    pub counter: u64,
    /// Correlation nonce for the idle query.
    pub nonce: String,
}

/// Errors while encoding, signing, or verifying a claim frame.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClaimFrameError {
    /// A variable-width field exceeds the protocol's u16 length bound.
    #[error("claim field {field} exceeds the u16 length bound")]
    FieldTooLong {
        /// Name of the oversized field.
        field: &'static str,
    },
    /// A paired peer's public key is not valid standard base64.
    #[error("paired peer public key is not valid base64")]
    InvalidPeerKeyEncoding,
    /// A paired peer's decoded public key is not 32 bytes.
    #[error("paired peer public key must be 32 bytes")]
    InvalidPeerKeyLength,
    /// A frame claimed an instance ID other than its paired signing identity.
    #[error("frame sender does not match the paired peer")]
    SenderMismatch,
    /// A request's asserted requester differs from the public key that signed it.
    #[error("claim requester does not match the signing peer")]
    RequesterMismatch,
    /// The frame is addressed to another paired instance.
    #[error("claim frame is addressed to another recipient")]
    RecipientMismatch,
    /// The recipient has restarted since the sender learned its epoch.
    #[error("claim frame uses a stale recipient epoch")]
    StaleRecipientEpoch,
    /// A request's inner replay identifiers disagree with its signed envelope.
    #[error("claim request replay identifiers disagree with the frame envelope")]
    RequestReplayMismatch,
    /// A pre-auth string exceeds the listener's bounded claim field size.
    #[error("claim frame contains an oversized pre-auth field")]
    OversizedPreauthField,
    /// An epoch is empty, all-zero, or not the fixed random-wire length.
    #[error("claim epoch must be a non-zero 16-byte value")]
    InvalidEpoch,
    /// The signature field is not valid standard base64.
    #[error("claim signature is not valid base64")]
    InvalidSignatureEncoding,
    /// The signature has the wrong Ed25519 length.
    #[error("claim signature has an invalid length")]
    InvalidSignatureLength,
    /// The signature is invalid for the canonical frame payload.
    #[error("claim signature verification failed")]
    InvalidSignature,
}

impl ClaimFrame {
    /// Sign a claim message using a paired instance identity.
    ///
    /// # Errors
    ///
    /// Returns an error when a signed string field cannot fit the u16 transcript bound.
    pub fn sign(
        identity: &InstanceIdentity,
        sender_epoch: String,
        recipient_instance_id: String,
        recipient_epoch: String,
        counter: u64,
        nonce: String,
        message: ClaimMessage,
    ) -> Result<Self, ClaimFrameError> {
        let mut frame = Self {
            sender_instance_id: identity.instance_id.clone(),
            sender_epoch,
            recipient_instance_id,
            recipient_epoch,
            counter,
            nonce,
            message,
            signature: String::new(),
        };
        frame.validate_pre_auth_fields()?;
        let payload = frame.canonical_signed_payload()?;
        frame.signature = STANDARD.encode(identity.signing_key.sign(&payload).to_bytes());
        Ok(frame)
    }

    /// Verify the frame before inspecting its message fields.
    ///
    /// # Errors
    ///
    /// Returns an error when the paired identity, signature encoding, or signature is invalid.
    pub fn verify(
        &self,
        peer: &PeerRecord,
        local_instance_id: &str,
        local_epoch: &str,
    ) -> Result<(), ClaimFrameError> {
        self.validate_pre_auth_fields()?;
        if self.sender_instance_id != peer.instance_id {
            return Err(ClaimFrameError::SenderMismatch);
        }
        let public_key = STANDARD
            .decode(&peer.ed25519_pub)
            .map_err(|_| ClaimFrameError::InvalidPeerKeyEncoding)?;
        let public_key: [u8; 32] = public_key
            .try_into()
            .map_err(|_| ClaimFrameError::InvalidPeerKeyLength)?;
        let verifying_key = VerifyingKey::from_bytes(&public_key)
            .map_err(|_| ClaimFrameError::InvalidPeerKeyLength)?;
        let signature = STANDARD
            .decode(&self.signature)
            .map_err(|_| ClaimFrameError::InvalidSignatureEncoding)?;
        let signature = Signature::from_slice(&signature)
            .map_err(|_| ClaimFrameError::InvalidSignatureLength)?;
        verifying_key
            .verify(&self.canonical_signed_payload()?, &signature)
            .map_err(|_| ClaimFrameError::InvalidSignature)?;
        if self.recipient_instance_id != local_instance_id {
            return Err(ClaimFrameError::RecipientMismatch);
        }
        if self.recipient_epoch != local_epoch {
            return Err(ClaimFrameError::StaleRecipientEpoch);
        }
        if let ClaimMessage::ClaimRequest(request) = &self.message {
            if request.requester_instance_id != self.sender_instance_id {
                return Err(ClaimFrameError::RequesterMismatch);
            }
            if request.counter != self.counter || request.nonce != self.nonce {
                return Err(ClaimFrameError::RequestReplayMismatch);
            }
        }
        Ok(())
    }

    /// Build the stable Ed25519 transcript for this frame.
    ///
    /// The transcript is deliberately independent of JSON field ordering so transport codecs can
    /// evolve without invalidating signatures.
    ///
    /// # Errors
    ///
    /// Returns an error when a variable-width field cannot fit its u16 length prefix.
    pub fn canonical_signed_payload(&self) -> Result<Vec<u8>, ClaimFrameError> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&CLAIM_PROTOCOL_VERSION.to_be_bytes());
        append_string(&mut payload, &self.sender_instance_id, "sender_instance_id")?;
        append_string(&mut payload, &self.sender_epoch, "sender_epoch")?;
        append_string(
            &mut payload,
            &self.recipient_instance_id,
            "recipient_instance_id",
        )?;
        append_string(&mut payload, &self.recipient_epoch, "recipient_epoch")?;
        payload.extend_from_slice(&self.counter.to_be_bytes());
        append_string(&mut payload, &self.nonce, "nonce")?;
        append_message(&mut payload, &self.message)?;
        Ok(payload)
    }

    fn validate_pre_auth_fields(&self) -> Result<(), ClaimFrameError> {
        const MAX_PREAUTH_FIELD_BYTES: usize = 1024;
        if [
            &self.sender_instance_id,
            &self.sender_epoch,
            &self.recipient_instance_id,
            &self.recipient_epoch,
            &self.nonce,
            &self.signature,
        ]
        .iter()
        .any(|field| field.len() > MAX_PREAUTH_FIELD_BYTES)
        {
            return Err(ClaimFrameError::OversizedPreauthField);
        }
        if [&self.sender_epoch, &self.recipient_epoch]
            .iter()
            .any(|epoch| epoch.len() != 16 || epoch.as_bytes().iter().all(|byte| *byte == 0))
        {
            return Err(ClaimFrameError::InvalidEpoch);
        }
        Ok(())
    }
}

/// Bounded replay state for a single paired peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayWindow {
    highest_counter: Option<u64>,
    nonces: VecDeque<String>,
    nonce_capacity: usize,
}

impl ReplayWindow {
    /// Create a replay window retaining at least one nonce.
    #[must_use]
    pub fn new(nonce_capacity: usize) -> Self {
        Self {
            highest_counter: None,
            nonces: VecDeque::with_capacity(nonce_capacity.max(1)),
            nonce_capacity: nonce_capacity.max(1),
        }
    }

    /// Accept a strictly increasing counter without nonce tracking.
    pub fn accept(&mut self, counter: u64) -> bool {
        if self
            .highest_counter
            .is_some_and(|highest| counter <= highest)
        {
            return false;
        }
        self.highest_counter = Some(counter);
        true
    }

    /// Accept a frame only when both its counter and nonce are fresh for this peer.
    pub fn accept_frame(&mut self, counter: u64, nonce: &str) -> bool {
        if self.nonces.iter().any(|seen| seen == nonce) || !self.accept(counter) {
            return false;
        }
        self.nonces.push_back(nonce.to_owned());
        if self.nonces.len() > self.nonce_capacity {
            self.nonces.pop_front();
        }
        true
    }
}

/// Compute the requester-visible bound for an owner release sequence.
///
/// The owner-supplied estimate includes blocking hook timeouts and any wake budget. The floor
/// reserves at least two polls plus one second for a confirming ownership read.
#[must_use]
pub fn release_deadline(
    owner_eta_hint: Duration,
    poll_interval: Duration,
    release_deadline_cap: Duration,
) -> Duration {
    let floor = poll_interval
        .saturating_mul(2)
        .saturating_add(Duration::from_secs(1));
    owner_eta_hint.max(floor).min(release_deadline_cap)
}

fn append_message(payload: &mut Vec<u8>, message: &ClaimMessage) -> Result<(), ClaimFrameError> {
    match message {
        ClaimMessage::ClaimRequest(request) => {
            append_string(payload, "claim_request", "message_type")?;
            append_string(payload, &request.display_identity, "display_identity")?;
            append_string(
                payload,
                &request.requester_instance_id,
                "requester_instance_id",
            )?;
            payload.extend_from_slice(&request.requester_input_code.to_be_bytes());
            payload.extend_from_slice(&request.counter.to_be_bytes());
            append_string(payload, &request.nonce, "request_nonce")
        }
        ClaimMessage::ClaimResponse(response) => {
            append_string(payload, "claim_response", "message_type")?;
            append_string(payload, &response.nonce, "response_nonce")?;
            append_verdict(payload, &response.verdict)
        }
        ClaimMessage::ReleaseFailed(release_failed) => {
            append_string(payload, "release_failed", "message_type")?;
            append_string(payload, &release_failed.nonce, "release_failed_nonce")?;
            append_string(payload, &release_failed.reason, "release_failed_reason")
        }
        ClaimMessage::ClaimAbort(abort) => {
            append_string(payload, "claim_abort", "message_type")?;
            append_string(payload, &abort.nonce, "abort_nonce")
        }
        ClaimMessage::IdleQuery(query) => {
            append_string(payload, "idle_query", "message_type")?;
            append_string(payload, &query.nonce, "idle_query_nonce")
        }
        ClaimMessage::IdleReport(report) => {
            append_string(payload, "idle_report", "message_type")?;
            payload.extend_from_slice(&report.idle_ms.to_be_bytes());
            payload.extend_from_slice(&report.counter.to_be_bytes());
            append_string(payload, &report.nonce, "idle_report_nonce")
        }
    }
}

fn append_verdict(payload: &mut Vec<u8>, verdict: &ClaimVerdict) -> Result<(), ClaimFrameError> {
    match verdict {
        ClaimVerdict::Accepted { eta_ms } => {
            append_string(payload, "accepted", "verdict")?;
            payload.extend_from_slice(&eta_ms.to_be_bytes());
            Ok(())
        }
        ClaimVerdict::NotOwner => append_string(payload, "not_owner", "verdict"),
        ClaimVerdict::Denied(reason) => {
            append_string(payload, "denied", "verdict")?;
            append_string(
                payload,
                &serde_json::to_string(reason).expect("enum serialization"),
                "reason",
            )
        }
        ClaimVerdict::Busy => append_string(payload, "busy", "verdict"),
    }
}

fn append_string(
    payload: &mut Vec<u8>,
    value: &str,
    field: &'static str,
) -> Result<(), ClaimFrameError> {
    let length = u16::try_from(value.len()).map_err(|_| ClaimFrameError::FieldTooLong { field })?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ClaimAbort, ClaimDeniedReason, ClaimFrame, ClaimFrameError, ClaimMessage, ClaimRequest,
        ClaimResponse, ClaimVerdict, Epoch, IdleQuery, IdleReport, ReleaseFailed, ReplayWindow,
        release_deadline,
    };
    use crate::peers::{InstanceIdentity, PeerRecord, instance_id_from_public_key};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signer as _, SigningKey};
    use std::time::Duration;

    fn identity(seed: u8) -> InstanceIdentity {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let verifying_key = signing_key.verifying_key();
        InstanceIdentity {
            instance_id: instance_id_from_public_key(&verifying_key.to_bytes()),
            signing_key,
            verifying_key,
        }
    }

    fn peer(identity: &InstanceIdentity) -> PeerRecord {
        PeerRecord {
            instance_id: identity.instance_id.clone(),
            ed25519_pub: STANDARD.encode(identity.verifying_key.as_bytes()),
            display_name: "test peer".to_owned(),
            paired_at: "2026-07-24T00:00:00Z".to_owned(),
            last_addr: None,
            claim_port: None,
        }
    }

    fn request() -> ClaimMessage {
        ClaimMessage::ClaimRequest(ClaimRequest {
            display_identity: "edid:acme:panel".to_owned(),
            requester_instance_id: identity(7).instance_id,
            requester_input_code: 15,
            counter: 9,
            nonce: "request-nonce".to_owned(),
        })
    }

    fn signed(message: ClaimMessage) -> ClaimFrame {
        let signer = identity(7);
        ClaimFrame::sign(
            &signer,
            "sender-epoch-000".to_owned(),
            "recipient-id".to_owned(),
            "recipient-epoch-".to_owned(),
            9,
            "frame-nonce".to_owned(),
            message,
        )
        .unwrap()
    }

    #[test]
    fn replay_window_rejects_non_increasing_counter() {
        let mut seen = ReplayWindow::new(64);
        assert!(seen.accept(7));
        assert!(!seen.accept(7));
        assert!(!seen.accept(6));
        assert!(seen.accept(8));
    }

    #[test]
    fn epoch_accepts_only_nonzero_fixed_width_values() {
        assert_eq!(
            Epoch::try_from("valid-epoch-0001").unwrap().as_str(),
            "valid-epoch-0001"
        );
        assert_eq!(Epoch::try_from("short"), Err(ClaimFrameError::InvalidEpoch));
        assert_eq!(
            Epoch::try_from("\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"),
            Err(ClaimFrameError::InvalidEpoch)
        );
    }

    #[test]
    fn release_deadline_is_floored_then_capped() {
        assert_eq!(
            release_deadline(
                Duration::from_millis(500),
                Duration::from_secs(2),
                Duration::from_secs(45),
            ),
            Duration::from_secs(5)
        );
        assert_eq!(
            release_deadline(
                Duration::from_secs(90),
                Duration::from_secs(2),
                Duration::from_secs(45),
            ),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn release_deadline_uses_hook_sum_between_floor_and_cap() {
        assert_eq!(
            release_deadline(
                Duration::from_secs(12),
                Duration::from_secs(2),
                Duration::from_secs(45),
            ),
            Duration::from_secs(12)
        );
    }

    #[test]
    fn signed_frame_verifies_and_eta_round_trips_over_json() {
        let signer = identity(7);
        let frame = ClaimFrame::sign(
            &signer,
            "sender-epoch-000".to_owned(),
            "recipient-id".to_owned(),
            "recipient-epoch-".to_owned(),
            10,
            "response-frame-nonce".to_owned(),
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce: "request-nonce".to_owned(),
                verdict: ClaimVerdict::Accepted { eta_ms: 12_345 },
            }),
        )
        .unwrap();
        let wire = serde_json::to_string(&frame).unwrap();
        let decoded: ClaimFrame = serde_json::from_str(&wire).unwrap();

        decoded
            .verify(&peer(&signer), "recipient-id", "recipient-epoch-")
            .unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn signature_rejects_tampering_with_every_signed_field() {
        let signer = identity(7);
        let peer = peer(&signer);
        let frame = signed(request());

        let mut sender = frame.clone();
        sender.sender_instance_id.push('x');
        assert!(
            sender
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut counter = frame.clone();
        counter.counter += 1;
        assert!(
            counter
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut nonce = frame.clone();
        nonce.nonce.push('x');
        assert!(
            nonce
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut display_identity = frame.clone();
        let ClaimMessage::ClaimRequest(request) = &mut display_identity.message else {
            unreachable!();
        };
        request.display_identity.push('x');
        assert!(
            display_identity
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut requester_instance_id = frame.clone();
        let ClaimMessage::ClaimRequest(request) = &mut requester_instance_id.message else {
            unreachable!();
        };
        request.requester_instance_id.push('x');
        assert!(
            requester_instance_id
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut input_code = frame.clone();
        let ClaimMessage::ClaimRequest(request) = &mut input_code.message else {
            unreachable!();
        };
        request.requester_input_code += 1;
        assert!(
            input_code
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut request_counter = frame.clone();
        let ClaimMessage::ClaimRequest(request) = &mut request_counter.message else {
            unreachable!();
        };
        request.counter += 1;
        assert!(
            request_counter
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );

        let mut request_nonce = frame;
        let ClaimMessage::ClaimRequest(request) = &mut request_nonce.message else {
            unreachable!();
        };
        request.nonce.push('x');
        assert!(
            request_nonce
                .verify(&peer, "recipient-id", "recipient-epoch-")
                .is_err()
        );
    }

    #[test]
    fn replay_window_rejects_duplicate_nonce_and_isolated_peer_counters() {
        let mut first_peer = ReplayWindow::new(2);
        let mut second_peer = ReplayWindow::new(2);

        assert!(first_peer.accept_frame(7, "first"));
        assert!(!first_peer.accept_frame(8, "first"));
        assert!(second_peer.accept_frame(7, "first"));
    }

    #[test]
    fn verification_rejects_relay_to_a_third_machine_and_requester_mismatch() {
        let signer = identity(7);
        let peer = peer(&signer);
        let frame = signed(request());

        assert_eq!(
            frame.verify(&peer, "third-machine", "recipient-epoch-"),
            Err(ClaimFrameError::RecipientMismatch)
        );

        let mut mismatched_requester = frame;
        let ClaimMessage::ClaimRequest(request) = &mut mismatched_requester.message else {
            unreachable!();
        };
        request.requester_instance_id = "forged-requester".to_owned();
        let payload = mismatched_requester.canonical_signed_payload().unwrap();
        mismatched_requester.signature =
            STANDARD.encode(signer.signing_key.sign(&payload).to_bytes());
        assert_eq!(
            mismatched_requester.verify(&peer, "recipient-id", "recipient-epoch-"),
            Err(ClaimFrameError::RequesterMismatch)
        );
    }

    #[test]
    fn restart_replay_is_rejected_and_stale_epoch_carries_a_retry_value() {
        let signer = identity(7);
        let peer = peer(&signer);
        let old_frame = signed(request());
        let mut before_restart = ReplayWindow::new(2);
        assert!(before_restart.accept_frame(old_frame.counter, &old_frame.nonce));
        let after_restart = ReplayWindow::new(2);

        assert_eq!(
            old_frame.verify(&peer, "recipient-id", "new-recipient-epoch"),
            Err(ClaimFrameError::StaleRecipientEpoch)
        );
        assert!(after_restart.highest_counter.is_none());

        let response = ClaimVerdict::Denied(ClaimDeniedReason::StaleEpoch {
            recipient_epoch: "new-recipient-epoch".to_owned(),
        });
        assert_eq!(
            serde_json::from_str::<ClaimVerdict>(&serde_json::to_string(&response).unwrap())
                .unwrap(),
            response
        );
    }

    #[test]
    fn denied_reason_accepts_unknown_future_wire_value() {
        let response: ClaimResponse = serde_json::from_str(
            r#"{"nonce":"request-nonce","verdict":{"verdict":"denied","reason":"future_reason"}}"#,
        )
        .unwrap();
        assert_eq!(
            response.verdict,
            ClaimVerdict::Denied(ClaimDeniedReason::Unknown)
        );
    }

    #[test]
    fn signed_layout_and_json_frames_match_golden_vectors() {
        let vectors = [
            request(),
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce: "request-nonce".to_owned(),
                verdict: ClaimVerdict::Accepted { eta_ms: 12_345 },
            }),
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce: "request-nonce".to_owned(),
                verdict: ClaimVerdict::Denied(ClaimDeniedReason::Unsupported),
            }),
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce: "request-nonce".to_owned(),
                verdict: ClaimVerdict::Busy,
            }),
            ClaimMessage::ClaimResponse(ClaimResponse {
                nonce: "request-nonce".to_owned(),
                verdict: ClaimVerdict::NotOwner,
            }),
            ClaimMessage::ClaimAbort(ClaimAbort {
                nonce: "request-nonce".to_owned(),
            }),
            ClaimMessage::ReleaseFailed(ReleaseFailed {
                nonce: "request-nonce".to_owned(),
                reason: "switch write failed".to_owned(),
            }),
            ClaimMessage::IdleQuery(IdleQuery {
                nonce: "request-nonce".to_owned(),
            }),
            ClaimMessage::IdleReport(IdleReport {
                idle_ms: 54_321,
                counter: 11,
                nonce: "request-nonce".to_owned(),
            }),
        ];

        let frames = vectors.map(signed);
        let json = frames
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let signed_bytes = frames
            .iter()
            .map(ClaimFrame::canonical_signed_payload)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let signed_payloads = signed_bytes
            .iter()
            .map(|payload| STANDARD.encode(payload))
            .collect::<Vec<_>>();
        assert_eq!(
            frames
                .iter()
                .map(|frame| &frame.signature)
                .collect::<Vec<_>>(),
            [
                "CMMg1TaFtlxJTS7iUoHLw0GxF4neBpGzKxTKLedoBoEBLgdEU9G96FI5QWlkZfhNGsKTpCinBSK0zgjFIUylAg==",
                "wpkub160jizA9HSRD9tm78JKObPeq7ruBFjUpvU08IHm9+SE3R9DnPWpCcz/hznpb21bV+hURE6M1fpeWDGiAw==",
                "w1uj02q0zbPF15kbD+YOJWez9ud4u9fmSRhhzjN72CHrUZKP4O1kniwsAUtTR1oO+YhYfBRR7qfYZUBa0xRzDg==",
                "CLHXYe50oomzpjMezwi/VJ2w6YD2nUNoaqlgqVoplO50zWmAdJSYH9TOpfdthH5GfFpqa0OvZRv0xHC3E6mKCA==",
                "tJsdQ4ofC3KY1vLdnxmHQkwcrBFunExJKrczS8qg2xGCDjl/SowHKWC6o7DwnFlfvFWe1XR8ILZeJgX95FgnAg==",
                "xOcWQ7IGrP1gCe/ckgZ9FF6f7UlFiW9kqZm1HIKMiIuHVGtWMycFnATSEtaPAmvQqvajxnoUWc0BAEOVbOrcBg==",
                "cJZBlF87wO9/4vdpIZ/n47sowSg/e+1aBUf7VfMvmkcsXVvF6L8XnDa5XY6mENyBp8JDNzhPuSnuDyDyrK10AQ==",
                "kQlKq6Y+fymxSe0ehFugJYYjlxlVVmfB1xYpdTv8JPz/N0ZUMJtuA523bKXbRPPAYVDFkYSKU7KIgzXFidRCCA==",
                "fklUu0IRcPhTVwYV+fPDXoQ686MUsBhDF14w1g4hm0rOYitWyZiTsjyuHQxG0A2mlNydIG/ItN+FNW3qi/nKAg==",
            ]
        );
        assert_eq!(signed_payloads.len(), 9);
        assert!(
            json.iter()
                .all(|frame| frame.contains("recipient_instance_id"))
        );
    }
}
