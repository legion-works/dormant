# Multi-machine pairing protocol — REV-2

Date: 2026-07-21
Status: ratified implementation contract for Tasks 11–14

## Decisions

### Pairing crypto: SPAKE2 plus persistent Ed25519 identity

Pairing uses `spake2` with the eight-character pairing code as the PAKE
password. A PAKE makes an incorrect code and an active man-in-the-middle fail
cryptographic key confirmation; it does not merely ask an operator to compare
text after an unauthenticated exchange. This follows the multimachine-coord
plan's explicit instruction to **reject bare key-exchange + compared code**.
SAS/compared-code pairing is therefore rejected.

Each instance has a persistent `ed25519-dalek` identity. After SPAKE2 produces
the shared key, peers exchange their Ed25519 public keys, then each sends an
HMAC-SHA256 key confirmation over the complete transcript. A mismatched MAC
aborts the session and leaves no peer record on disk. The identity and
protocol-version fields in that transcript prevent identity substitution and
version downgrade from being accepted as a paired session.

`opaque-ke` is rejected because it has the wrong shape: it is an augmented
PAKE with a client and a server holding a stored verifier. Dormant pairing is a
symmetric peer-to-peer operation between two instances; it does not need a
long-lived verifier service.

### Discovery: `mdns-sd`

Discovery and advertisement use the pure-Rust `mdns-sd` crate with service type
`_dormant._tcp.local.`. It supports both browse and advertise without a native
Bonjour or Avahi dependency. The 2026-07-21 dependency probe completed
`cargo check -p dormantd --all-features` on Linux and macOS with
`mdns-sd v0.20.2` present. Discovery is an address-selection convenience only;
it is never a heartbeat or an online-health signal.

`zeroconf` is rejected because it relies on native Bonjour/Avahi services.
`libmdns` is rejected because it does not provide the required browse path.

### Listener, code, and roles

The machine whose operator opens Pair is the **responder**: it displays a code,
advertises itself, and listens only on selected LAN interfaces for the pairing
window. The other machine is the **initiator**: it discovers the responder via
mDNS, receives the code from its operator, and initiates SPAKE2 over TCP.

The code is eight Crockford Base32 characters (40 random bits), generated once
per window from the operating system RNG. It expires after at most five
minutes, permits at most ten `PairHello` attempts, and is consumed after one
successful confirmation. The listener is closed and its mDNS service removed
when the window expires, succeeds, or is cancelled. It is not an always-on
port.

## Persistent identity and peer store

`dormant_core::paths::state_dir()/instance-key` contains the private identity
material. It is created with an atomic write and mode `0600`; its raw private
bytes never enter logs, IPC replies, or web responses. The public instance ID
is the base64url-without-padding encoding of the Ed25519 verifying key.

```rust
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

pub struct InstanceIdentity {
    pub instance_id: String,
    pub signing_key: SigningKey,
    pub verifying_key: VerifyingKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStore {
    pub version: u32,
    #[serde(default)]
    pub peers: Vec<PeerRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub instance_id: String,
    pub ed25519_pub: String,
    pub display_name: String,
    pub paired_at: String,
}
```

`state_dir()/peers.json` is written atomically with mode `0600` and has this
shape:

```json
{
  "version": 1,
  "peers": [
    {
      "instance_id": "base64url-ed25519-public-key",
      "ed25519_pub": "base64-standard-ed25519-public-key",
      "display_name": "Office Mac",
      "paired_at": "2026-07-21T12:00:00Z"
    }
  ]
}
```

`paired_at` is RFC 3339 UTC. Additive peer-store fields use `#[serde(default)]`;
a breaking shape change increments `version` and gets an explicit migration.
Duplicate `instance_id` records are replaced only after a fresh successful
pairing with that same verified public key; an ID with a different key is a
hard error requiring operator removal of the prior peer.

## Wire protocol

All TCP messages are a four-byte unsigned big-endian payload length followed by
one UTF-8 `serde_json` encoding of `PairFrame`. The length excludes the prefix
and must be in `1..=65_536`; an invalid length, invalid JSON, oversized value,
or unexpected frame order closes the connection. Byte fields are base64 strings
in JSON. TCP is used only after mDNS supplies the responder address and port.

```rust
use serde::{Deserialize, Serialize};

pub const PAIR_PROTOCOL_VERSION: u16 = 2;
pub const MAX_PAIR_FRAME_BYTES: u32 = 65_536;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverAnnounce {
    pub protocol_version: u16,
    pub instance_id: String,
    pub display_name: String,
    pub pairing_port: u16,
    pub window_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairRole {
    Initiator,
    Responder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairFrame {
    DiscoverAnnounce(DiscoverAnnounce),
    PairHello {
        protocol_version: u16,
        role: PairRole,
        instance_id: String,
        window_id: String,
        nonce: String,
    },
    Spake2Msg1 { message: String },
    Spake2Msg2 { message: String },
    IdentityExchange { ed25519_pub: String },
    KeyConfirm { mac: String },
    PairResult { accepted: bool, error: Option<PairError> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairError {
    Cancelled,
    CodeExpired,
    AttemptLimit,
    ProtocolVersion,
    InvalidFrame,
    KeyConfirmation,
    InstanceIdConflict,
}
```

`DiscoverAnnounce` maps directly to mDNS TXT records named `v`, `instance_id`,
`display_name`, `pairing_port`, and `window_id`; the advertised service port is
the same `pairing_port`. Discovery data is not proof of identity and is not
persisted. `PairHello` verifies the protocol version before SPAKE2 begins.

The initiator sends `PairHello`, `Spake2Msg1`, and `IdentityExchange`; the
responder sends `Spake2Msg2` and `IdentityExchange`; both then send
`KeyConfirm` and a terminal `PairResult`. `PairResult { accepted: true,
error: None }` is sent only after receiving the peer's valid confirmation.

Each role generates a new 32-byte nonce for every connection. The responder
records both nonces for the window lifetime and rejects reuse. A code window is
single-use: once a session succeeds, every later frame for its `window_id` is
rejected. Closing a failed connection does not reset its counted attempt.

For a session key `K` returned by SPAKE2, each side computes
`HMAC-SHA256(K, transcript)`. `transcript` is the unambiguous concatenation of
the following values, in this order: big-endian `protocol_version`; length-
prefixed ASCII role labels `initiator` then `responder`; length-prefixed UTF-8
initiator and responder instance IDs; 32 raw bytes of initiator then responder
Ed25519 public keys; then 32 raw bytes of initiator then responder nonces. A
peer accepts only an exact 32-byte MAC equality. The version is therefore bound
before a downgrade can become a paired state.

Operator cancellation cancels the local task, removes the advertisement, closes
the listener or TCP stream, and sends `PairResult { accepted: false,
error: Some(Cancelled) }` when the stream remains writable. It never writes a
peer. If both instances initiate concurrently, lexicographically lower
`instance_id` yields: it cancels its outbound attempt and accepts the higher-ID
peer's inbound attempt; the higher-ID initiator continues. This gives one
deterministic session without a timing race.

## Configuration

The complete `[coordination]` contract is:

| Key | Type | Default | Bound / behavior |
|---|---|---|---|
| `enabled` | bool | `false` | Enables mDNS, pairing listener, pairing IPC, and pairing web routes. It never disables local `0x60` ownership polling. |
| `poll_interval` | humantime duration | `"2s"` | Existing shared-display poll cadence; minimum `"1s"`, no upper ceiling. |
| `pairing_port` | u16 | `0` | `0` asks the OS for an ephemeral port; otherwise `1..=65535`. The resolved port is published only in the pairing-window TXT record. |
| `pairing_window` | humantime duration | `"5m"` | `"30s"..="15m"`; this is the only time that a LAN listener and mDNS service may exist. |

The routes compile with the existing `web-ui` feature; no new Cargo feature is
introduced. When `enabled = false`, every instance-pairing route returns
`403` with `{"error":"coordination_disabled"}` and the daemon starts neither
the mDNS browser nor a pairing listener. The existing local `0x60` polling
continues regardless of this setting.

## LAN exposure and threat model

The responder binds only the selected non-loopback LAN interface addresses
while a pairing window is live. It does not bind a wildcard address or retain a
listening port after the window. The pairing mDNS service is likewise published
only for that interval.

On a hostile LAN, an attacker can see the mDNS advertisement; advertising only
during the pairing window limits that exposure. An attacker can connect to the
advertised port during the window; SPAKE2 requires the code and the responder
enforces the ten-attempt limit. An attacker can proxy or modify SPAKE2 traffic;
the transcript-bound key confirmation fails. An attacker can offer an older
protocol version; the version check plus its inclusion in the confirmation MAC
prevents a downgrade from succeeding.

An on-path attacker during the active window can drop, delay, or flood traffic
and deny service to pairing. That denial of service is not defended; no peer is
persisted unless confirmation completes.

## Operator interfaces

Instance pairing follows the Samsung wizard's single-flight, asynchronous
shape, but all new endpoints are loopback-only and gated by
`coordination.enabled`:

| Route | Request | Response |
|---|---|---|
| `POST /api/pair/instance` | `{"display_name":"Office Mac"}` | `202 {"pair_id":"…","code":"ABCD1234","expires_at":"RFC3339"}`; opens the responder window. |
| `GET /api/pair/instance/{pair_id}` | none | `200 {"state":"pairing|paired|timeout|cancelled|error","detail":null|string}`. |
| `POST /api/pair/instance/{pair_id}/cancel` | none | `202 {"state":"cancelled"}`; removes the listener and advertisement. |

The code is shown only by the POST response while the requester is local. It is
never written to `peers.json`, tracing, or daemon IPC logs. The CLI entry point
on the discovering machine is:

```text
dormantctl pair instance <name> --code <CROCKFORD_BASE32_CODE>
```

`<name>` matches the current mDNS `display_name`; duplicate discoveries require
the CLI to list their instance IDs and select one explicitly. The CLI sends a
daemon IPC `CoordinationPairJoin { display_name, instance_id, code }` request.
The web POST uses `CoordinationPairOpen { display_name }`; status and cancel
use `CoordinationPairStatus { pair_id }` and `CoordinationPairCancel { pair_id
}`. IPC replies carry only `pair_id`, public status, and public peer identity;
they never carry a pairing code after join submission or private key material.

## Task 9 wire-contract ratification

Task 9's display-coordination wire contract is ratified unchanged: its
additive `scope`, `owned`, `observed_input_code`, and `panel_state` fields keep
serde defaults. Unknown or stale remote data receives private defaults and
holds the last known state. Pairing does not alter that compatibility or
fail-safe policy.

## Runtime feature policy

Pairing transport, mDNS, identity persistence, and IPC live in `dormantd`, the
I/O crate. Pure protocol types may live in `dormant-core`. The existing
`web-ui` feature only compiles the loopback HTTP surface; it does not control
whether the daemon can pair. Runtime `coordination.enabled` is the sole switch
for exposure, while local display ownership polling remains available without
pairing.
