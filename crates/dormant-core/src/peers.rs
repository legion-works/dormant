//! Persistent instance identities, paired-peer records, and pairing wire types.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose};
use ed25519_dalek::{SigningKey, VerifyingKey};
use getrandom::{SysRng, rand_core::UnwrapErr};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The only peer-store schema version understood by this build.
pub const PEER_STORE_VERSION: u32 = 1;
/// The pairing protocol version bound into every confirmation transcript.
pub const PAIR_PROTOCOL_VERSION: u16 = 2;
/// Maximum accepted JSON frame payload size for the pairing transport.
pub const MAX_PAIR_FRAME_BYTES: u32 = 65_536;
/// Maximum permitted on-disk peer-store size before it is rejected without reading.
pub const MAX_PEER_STORE_BYTES: u64 = 64 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Persistent Ed25519 identity for one dormant instance.
pub struct InstanceIdentity {
    /// Stable base64url instance identifier derived from `verifying_key`.
    pub instance_id: String,
    /// Private signing key. It is never serialized or included in diagnostics.
    pub signing_key: SigningKey,
    /// Public verifying key corresponding to `signing_key`.
    pub verifying_key: VerifyingKey,
}

impl fmt::Debug for InstanceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstanceIdentity")
            .field("instance_id", &self.instance_id)
            .field("signing_key", &"<redacted>")
            .field("verifying_key", &self.verifying_key)
            .finish()
    }
}

/// Versioned durable record of all paired instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStore {
    /// On-disk peer-store schema version.
    pub version: u32,
    /// Paired instances.
    #[serde(default)]
    pub peers: Vec<PeerRecord>,
}

/// Public identity and operator-selected label for one paired instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    /// Base64url-without-padding encoding of the peer's Ed25519 public key.
    pub instance_id: String,
    /// Standard-base64 encoding of the peer's Ed25519 public key.
    pub ed25519_pub: String,
    /// Operator-selected peer label.
    pub display_name: String,
    /// RFC 3339 UTC timestamp when the pairing completed.
    pub paired_at: String,
}

/// mDNS discovery data carried into a pairing attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverAnnounce {
    /// Advertised pairing protocol version.
    pub protocol_version: u16,
    /// Advertised public instance identifier.
    pub instance_id: String,
    /// Advertised operator-selected display name.
    pub display_name: String,
    /// Responder TCP port for this pairing window.
    pub pairing_port: u16,
    /// Identifier for this short-lived pairing window.
    pub window_id: String,
}

/// The pairing participant's fixed protocol role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairRole {
    /// The machine that discovers and opens the TCP connection.
    Initiator,
    /// The machine that displays the code and accepts the TCP connection.
    Responder,
}

/// One JSON frame exchanged by pairing peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairFrame {
    /// mDNS discovery data.
    DiscoverAnnounce(DiscoverAnnounce),
    /// First transport message, sent by the initiator.
    PairHello {
        /// Claimed protocol version.
        protocol_version: u16,
        /// Sender's pairing role.
        role: PairRole,
        /// Sender's public instance identifier.
        instance_id: String,
        /// Sender's operator-selected display name.
        display_name: String,
        /// Pairing-window identifier.
        window_id: String,
        /// Base64-encoded 32-byte connection nonce.
        nonce: String,
    },
    /// First SPAKE2 message.
    Spake2Msg1 {
        /// Base64-encoded SPAKE2 message bytes.
        message: String,
    },
    /// Second SPAKE2 message.
    Spake2Msg2 {
        /// Base64-encoded SPAKE2 message bytes.
        message: String,
    },
    /// Base64-encoded Ed25519 public key sent after SPAKE2 completes.
    IdentityExchange {
        /// Base64-encoded Ed25519 public key.
        ed25519_pub: String,
    },
    /// Transcript HMAC confirmation.
    KeyConfirm {
        /// Base64-encoded HMAC-SHA256 output.
        mac: String,
    },
    /// Terminal pairing result.
    PairResult {
        /// Whether the sender accepts the pairing.
        accepted: bool,
        /// Failure reason when `accepted` is false.
        error: Option<PairError>,
    },
}

/// Public pairing failure reason exchanged in a terminal frame.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairError {
    /// The operator cancelled the pairing window.
    Cancelled,
    /// The short-lived pairing code window expired.
    CodeExpired,
    /// Too many `PairHello` attempts were received.
    AttemptLimit,
    /// The peer sent an unsupported protocol version.
    ProtocolVersion,
    /// The peer sent an invalid frame or invalid frame ordering.
    InvalidFrame,
    /// Transcript confirmation MACs did not agree.
    KeyConfirmation,
    /// A claimed instance identifier conflicts with its public key.
    InstanceIdConflict,
}

/// Errors while loading or persisting an identity or peer store.
#[derive(Debug, thiserror::Error)]
pub enum PeerStoreError {
    /// A persisted peer store violates its security or schema invariants.
    #[error("peer_store_invalid: {detail}")]
    Invalid {
        /// Public validation failure detail.
        detail: String,
    },
    /// I/O prevented a persistence operation.
    #[error("peer_store_io: {operation}: {source}")]
    Io {
        /// Stable operation label.
        operation: &'static str,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// A peer record tries to pair this instance with itself.
    #[error("peer_store_self_pairing")]
    SelfPairing,
    /// A transcript string exceeds its u16 byte-length field.
    #[error("peer_store_invalid: transcript field '{field}' exceeds 65535 bytes")]
    TranscriptFieldTooLong {
        /// Name of the oversized field.
        field: &'static str,
    },
}

/// Load the persistent identity in `state_dir`, creating it securely if absent.
///
/// # Errors
///
/// Returns an error when the key cannot be safely read or atomically persisted.
pub fn load_or_create_identity(state_dir: &Path) -> Result<InstanceIdentity, PeerStoreError> {
    fs::create_dir_all(state_dir).map_err(|source| PeerStoreError::Io {
        operation: "create identity directory",
        source,
    })?;
    let key_path = state_dir.join("instance-key");

    if key_path.exists() {
        ensure_private_permissions(&key_path)?;
        let bytes = fs::read(&key_path).map_err(|source| PeerStoreError::Io {
            operation: "read instance key",
            source,
        })?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| PeerStoreError::Invalid {
            detail: "instance-key must contain exactly 32 bytes".to_owned(),
        })?;
        return Ok(identity_from_signing_key(SigningKey::from_bytes(&bytes)));
    }

    let signing_key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let secret = Zeroizing::new(signing_key.to_bytes());
    atomic_write(&key_path, |file| file.write_all(&*secret))?;
    Ok(identity_from_signing_key(signing_key))
}

/// Load and validate a versioned peer store, returning an empty current store if absent.
///
/// # Errors
///
/// Returns an error for insecure file permissions, unsupported versions, malformed public
/// keys, duplicate identities, or any identity/key mismatch.
pub fn load_peer_store(path: &Path) -> Result<PeerStore, PeerStoreError> {
    if !path.exists() {
        return Ok(PeerStore {
            version: PEER_STORE_VERSION,
            peers: Vec::new(),
        });
    }

    ensure_private_permissions(path)?;
    let length = fs::metadata(path)
        .map_err(|source| PeerStoreError::Io {
            operation: "inspect peer store size",
            source,
        })?
        .len();
    if length > MAX_PEER_STORE_BYTES {
        return Err(PeerStoreError::Invalid {
            detail: format!("peers.json exceeds {MAX_PEER_STORE_BYTES} bytes"),
        });
    }
    let bytes = fs::read(path).map_err(|source| PeerStoreError::Io {
        operation: "read peer store",
        source,
    })?;
    let store = serde_json::from_slice(&bytes).map_err(|error| PeerStoreError::Invalid {
        detail: format!("cannot parse peers.json: {error}"),
    })?;
    validate_peer_store(&store)?;
    Ok(store)
}

/// Insert or update a peer record without discarding unrelated paired instances.
///
/// The store's sibling `instance-key` identifies the local machine, preventing a
/// self-pairing record from becoming durable.
///
/// # Errors
///
/// Returns an error if the store or record violates pairing invariants, or persistence fails.
pub fn upsert_peer(path: &Path, record: PeerRecord) -> Result<(), PeerStoreError> {
    validate_peer_record(&record)?;
    let state_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let identity = load_or_create_identity(state_dir)?;
    if record.instance_id == identity.instance_id {
        return Err(PeerStoreError::SelfPairing);
    }

    let mut store = load_peer_store(path)?;
    if let Some(existing) = store
        .peers
        .iter_mut()
        .find(|existing| existing.instance_id == record.instance_id)
    {
        if existing.ed25519_pub != record.ed25519_pub {
            return Err(PeerStoreError::Invalid {
                detail: "peer instance_id conflicts with a different public key".to_owned(),
            });
        }
        *existing = record;
    } else {
        store.peers.push(record);
    }

    let bytes = serde_json::to_vec_pretty(&store).map_err(|error| PeerStoreError::Invalid {
        detail: format!("cannot serialize peers.json: {error}"),
    })?;
    atomic_write(path, |file| file.write_all(&bytes))
}

/// Serialize the exact canonical pairing confirmation transcript.
///
/// The fixed role labels precede identities because both peers must MAC the same role ordering,
/// not whichever order their local connection observed.
///
/// # Errors
///
/// Returns an error if any UTF-8 field cannot fit the protocol's u16 length prefix.
#[allow(clippy::too_many_arguments)]
pub fn build_pairing_transcript(
    protocol_version: u16,
    initiator_instance_id: &str,
    responder_instance_id: &str,
    initiator_display_name: &str,
    responder_display_name: &str,
    initiator_public_key: &[u8; 32],
    responder_public_key: &[u8; 32],
    initiator_nonce: &[u8; 32],
    responder_nonce: &[u8; 32],
) -> Result<Vec<u8>, PeerStoreError> {
    let mut transcript = Vec::with_capacity(
        2 + 4 * 2
            + "initiator".len()
            + "responder".len()
            + initiator_instance_id.len()
            + responder_instance_id.len()
            + initiator_display_name.len()
            + responder_display_name.len()
            + 128,
    );
    transcript.extend_from_slice(&protocol_version.to_be_bytes());
    append_length_prefixed(&mut transcript, "initiator", "initiator role")?;
    append_length_prefixed(&mut transcript, "responder", "responder role")?;
    append_length_prefixed(
        &mut transcript,
        initiator_instance_id,
        "initiator instance_id",
    )?;
    append_length_prefixed(
        &mut transcript,
        responder_instance_id,
        "responder instance_id",
    )?;
    append_length_prefixed(
        &mut transcript,
        initiator_display_name,
        "initiator display_name",
    )?;
    append_length_prefixed(
        &mut transcript,
        responder_display_name,
        "responder display_name",
    )?;
    transcript.extend_from_slice(initiator_public_key);
    transcript.extend_from_slice(responder_public_key);
    transcript.extend_from_slice(initiator_nonce);
    transcript.extend_from_slice(responder_nonce);
    Ok(transcript)
}

fn identity_from_signing_key(signing_key: SigningKey) -> InstanceIdentity {
    let verifying_key = signing_key.verifying_key();
    let instance_id = instance_id_from_public_key(&verifying_key.to_bytes());
    InstanceIdentity {
        instance_id,
        signing_key,
        verifying_key,
    }
}

fn validate_peer_store(store: &PeerStore) -> Result<(), PeerStoreError> {
    if store.version > PEER_STORE_VERSION {
        return Err(PeerStoreError::Invalid {
            detail: format!("unsupported peers.json version {}", store.version),
        });
    }

    let mut seen_ids = std::collections::HashSet::new();
    for record in &store.peers {
        validate_peer_record(record)?;
        if !seen_ids.insert(&record.instance_id) {
            return Err(PeerStoreError::Invalid {
                detail: "duplicate peer instance_id".to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_peer_record(record: &PeerRecord) -> Result<(), PeerStoreError> {
    let public_key = general_purpose::STANDARD
        .decode(&record.ed25519_pub)
        .map_err(|_| PeerStoreError::Invalid {
            detail: "peer ed25519_pub is not valid base64".to_owned(),
        })?;
    let public_key: [u8; 32] = public_key.try_into().map_err(|_| PeerStoreError::Invalid {
        detail: "peer ed25519_pub must decode to exactly 32 bytes".to_owned(),
    })?;
    VerifyingKey::from_bytes(&public_key).map_err(|_| PeerStoreError::Invalid {
        detail: "peer ed25519_pub is not a valid Ed25519 public key".to_owned(),
    })?;
    if record.instance_id != instance_id_from_public_key(&public_key) {
        return Err(PeerStoreError::Invalid {
            detail: "peer instance_id does not match ed25519_pub".to_owned(),
        });
    }
    Ok(())
}

/// Encode an Ed25519 public key as its stable base64url instance identifier.
#[must_use]
pub fn instance_id_from_public_key(public_key: &[u8; 32]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(public_key)
}

fn append_length_prefixed(
    transcript: &mut Vec<u8>,
    value: &str,
    field: &'static str,
) -> Result<(), PeerStoreError> {
    let length =
        u16::try_from(value.len()).map_err(|_| PeerStoreError::TranscriptFieldTooLong { field })?;
    transcript.extend_from_slice(&length.to_be_bytes());
    transcript.extend_from_slice(value.as_bytes());
    Ok(())
}

fn atomic_write<F>(path: &Path, write: F) -> Result<(), PeerStoreError>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| PeerStoreError::Io {
        operation: "create peer store directory",
        source,
    })?;
    let temp_path = unique_temp_path(parent, path.file_name().unwrap_or_default());

    let result = (|| {
        let mut file = create_private_file(&temp_path)?;
        write(&mut file).map_err(|source| PeerStoreError::Io {
            operation: "write temporary peer store",
            source,
        })?;
        file.flush().map_err(|source| PeerStoreError::Io {
            operation: "flush temporary peer store",
            source,
        })?;
        file.sync_all().map_err(|source| PeerStoreError::Io {
            operation: "sync temporary peer store",
            source,
        })?;
        fs::rename(&temp_path, path).map_err(|source| PeerStoreError::Io {
            operation: "rename temporary peer store",
            source,
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn create_private_file(path: &Path) -> Result<File, PeerStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| PeerStoreError::Io {
                operation: "create temporary peer store",
                source,
            })
    }

    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| PeerStoreError::Io {
                operation: "create temporary peer store",
                source,
            })
    }
}

fn unique_temp_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        ".{}.tmp.{}.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        nanos,
        sequence
    ))
}

fn ensure_private_permissions(path: &Path) -> Result<(), PeerStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(path)
            .map_err(|source| PeerStoreError::Io {
                operation: "inspect private file permissions",
                source,
            })?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(PeerStoreError::Invalid {
                detail: format!("'{}' is readable by group or other", path.display()),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use base64::{Engine as _, engine::general_purpose};
    use ed25519_dalek::SigningKey;

    use super::*;

    fn test_record(seed: u8, display_name: &str) -> PeerRecord {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let verifying_key = signing_key.verifying_key().to_bytes();
        PeerRecord {
            instance_id: general_purpose::URL_SAFE_NO_PAD.encode(verifying_key),
            ed25519_pub: general_purpose::STANDARD.encode(verifying_key),
            display_name: display_name.to_owned(),
            paired_at: "2026-07-21T12:00:00Z".to_owned(),
        }
    }

    fn peers_path(dir: &Path) -> std::path::PathBuf {
        dir.join("peers.json")
    }

    fn write_private_peer_store(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn identity_is_stable_across_reload() {
        let dir = tempfile::tempdir().unwrap();

        let first = load_or_create_identity(dir.path()).unwrap();
        let second = load_or_create_identity(dir.path()).unwrap();

        assert_eq!(first.instance_id, second.instance_id);
        assert_eq!(first.verifying_key, second.verifying_key);
    }

    #[test]
    fn peer_store_round_trips_versioned_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let record = test_record(7, "Office Mac");

        upsert_peer(&path, record.clone()).unwrap();
        let store = load_peer_store(&path).unwrap();

        assert_eq!(store.version, PEER_STORE_VERSION);
        assert_eq!(store.peers.len(), 1);
        assert_eq!(store.peers[0].instance_id, record.instance_id);
        assert_eq!(store.peers[0].ed25519_pub, record.ed25519_pub);
        assert_eq!(store.peers[0].display_name, record.display_name);
        assert_eq!(store.peers[0].paired_at, record.paired_at);
    }

    #[cfg(unix)]
    #[test]
    fn peer_store_creates_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());

        upsert_peer(&path, test_record(8, "Desk")).unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn peer_store_rejects_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let identity_dir = tempfile::tempdir().unwrap();
        load_or_create_identity(identity_dir.path()).unwrap();
        std::fs::set_permissions(
            identity_dir.path().join("instance-key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(load_or_create_identity(identity_dir.path()).is_err());

        let peer_dir = tempfile::tempdir().unwrap();
        let path = peers_path(peer_dir.path());
        let store = PeerStore {
            version: PEER_STORE_VERSION,
            peers: vec![test_record(9, "TV")],
        };
        write_private_peer_store(&path, &serde_json::to_vec(&store).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_peer_store(&path).is_err());
    }

    #[test]
    fn upsert_preserves_unrelated_peers() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let first = test_record(10, "Office Mac");
        let second = test_record(11, "Living room TV");
        let replacement = PeerRecord {
            display_name: "Renamed office".to_owned(),
            ..first.clone()
        };

        upsert_peer(&path, first).unwrap();
        upsert_peer(&path, second.clone()).unwrap();
        upsert_peer(&path, replacement.clone()).unwrap();
        let store = load_peer_store(&path).unwrap();

        assert_eq!(store.peers.len(), 2);
        assert!(store.peers.iter().any(|record| {
            record.instance_id == replacement.instance_id
                && record.display_name == replacement.display_name
        }));
        assert!(store.peers.iter().any(|record| {
            record.instance_id == second.instance_id && record.display_name == second.display_name
        }));
    }

    #[test]
    fn atomic_write_cleans_temp_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());

        let result = atomic_write(&path, |_| {
            Err(std::io::Error::other("injected write failure"))
        });

        assert!(result.is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));
    }

    #[test]
    fn private_key_debug_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let identity = load_or_create_identity(dir.path()).unwrap();
        let key_bytes = identity.signing_key.to_bytes();
        let byte_debug = format!("{key_bytes:?}");
        let mut key_hex = String::new();
        for byte in key_bytes {
            use std::fmt::Write as _;

            write!(&mut key_hex, "{byte:02x}").unwrap();
        }
        let debug = format!("{identity:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&byte_debug));
        assert!(!debug.contains(&key_hex));
    }

    #[test]
    fn peer_store_rejects_duplicate_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let record = test_record(12, "Office Mac");
        let store = PeerStore {
            version: PEER_STORE_VERSION,
            peers: vec![record.clone(), record],
        };
        write_private_peer_store(&path, &serde_json::to_vec(&store).unwrap());

        let error = load_peer_store(&path).unwrap_err();

        assert!(error.to_string().contains("peer_store_invalid"));
    }

    #[test]
    fn peer_store_rejects_mismatched_instance_id_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let mut record = test_record(13, "Office Mac");
        record.instance_id = test_record(14, "Other").instance_id;
        let store = PeerStore {
            version: PEER_STORE_VERSION,
            peers: vec![record],
        };
        write_private_peer_store(&path, &serde_json::to_vec(&store).unwrap());

        assert!(load_peer_store(&path).is_err());
    }

    #[test]
    fn peer_store_rejects_malformed_pubkey() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let store = PeerStore {
            version: PEER_STORE_VERSION,
            peers: vec![PeerRecord {
                instance_id: "not-an-instance-id".to_owned(),
                ed25519_pub: "not base64".to_owned(),
                display_name: "Office Mac".to_owned(),
                paired_at: "2026-07-21T12:00:00Z".to_owned(),
            }],
        };
        write_private_peer_store(&path, &serde_json::to_vec(&store).unwrap());

        assert!(load_peer_store(&path).is_err());
    }

    #[test]
    fn peer_store_rejects_future_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let store = PeerStore {
            version: PEER_STORE_VERSION + 1,
            peers: Vec::new(),
        };
        write_private_peer_store(&path, &serde_json::to_vec(&store).unwrap());

        assert!(load_peer_store(&path).is_err());
    }

    #[test]
    fn peer_store_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let padding = "x".repeat(70 * 1024);
        write_private_peer_store(
            &path,
            format!(r#"{{"version":1,"peers":[],"padding":"{padding}"}}"#).as_bytes(),
        );

        let error = load_peer_store(&path).unwrap_err();

        assert!(error.to_string().contains("peer_store_invalid"));
    }

    #[test]
    fn upsert_rejects_self_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let path = peers_path(dir.path());
        let identity = load_or_create_identity(dir.path()).unwrap();
        let record = PeerRecord {
            instance_id: identity.instance_id,
            ed25519_pub: general_purpose::STANDARD.encode(identity.verifying_key.to_bytes()),
            display_name: "This machine".to_owned(),
            paired_at: "2026-07-21T12:00:00Z".to_owned(),
        };

        assert!(upsert_peer(&path, record).is_err());
    }

    #[derive(Clone)]
    struct TranscriptInputs {
        protocol_version: u16,
        initiator_instance_id: String,
        responder_instance_id: String,
        initiator_display_name: String,
        responder_display_name: String,
        initiator_public_key: [u8; 32],
        responder_public_key: [u8; 32],
        initiator_nonce: [u8; 32],
        responder_nonce: [u8; 32],
    }

    impl TranscriptInputs {
        fn canonical() -> Self {
            Self {
                protocol_version: PAIR_PROTOCOL_VERSION,
                initiator_instance_id: "initiator-id".to_owned(),
                responder_instance_id: "responder-id".to_owned(),
                initiator_display_name: "Initiator display".to_owned(),
                responder_display_name: "Responder display".to_owned(),
                initiator_public_key: [1; 32],
                responder_public_key: [2; 32],
                initiator_nonce: [3; 32],
                responder_nonce: [4; 32],
            }
        }

        fn bytes(&self) -> Vec<u8> {
            build_pairing_transcript(
                self.protocol_version,
                &self.initiator_instance_id,
                &self.responder_instance_id,
                &self.initiator_display_name,
                &self.responder_display_name,
                &self.initiator_public_key,
                &self.responder_public_key,
                &self.initiator_nonce,
                &self.responder_nonce,
            )
            .unwrap()
        }
    }

    fn transcript() -> Vec<u8> {
        TranscriptInputs::canonical().bytes()
    }

    #[test]
    fn transcript_is_deterministic() {
        assert_eq!(transcript(), transcript());
    }

    #[test]
    fn transcript_matches_golden_vector() {
        let expected_parts: &[&[u8]] = &[
            &[0, 2, 0, 9],
            b"initiator",
            &[0, 9],
            b"responder",
            &[0, 12],
            b"initiator-id",
            &[0, 12],
            b"responder-id",
            &[0, 17],
            b"Initiator display",
            &[0, 17],
            b"Responder display",
            &[1; 32],
            &[2; 32],
            &[3; 32],
            &[4; 32],
        ];
        let expected = expected_parts.concat();

        assert_eq!(transcript(), expected);
    }

    #[test]
    fn transcript_binds_each_field() {
        let input = TranscriptInputs::canonical();
        let original = input.bytes();
        let mut changed = Vec::new();

        let mut variant = input.clone();
        variant.protocol_version = 3;
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.initiator_instance_id = "other-initiator".to_owned();
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.responder_instance_id = "other-responder".to_owned();
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.initiator_display_name = "Other initiator".to_owned();
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.responder_display_name = "Other responder".to_owned();
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.initiator_public_key = [5; 32];
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.responder_public_key = [6; 32];
        changed.push(variant.bytes());
        let mut variant = input.clone();
        variant.initiator_nonce = [7; 32];
        changed.push(variant.bytes());
        let mut variant = input;
        variant.responder_nonce = [8; 32];
        changed.push(variant.bytes());

        for changed in changed {
            assert_ne!(original, changed);
        }
    }
}
