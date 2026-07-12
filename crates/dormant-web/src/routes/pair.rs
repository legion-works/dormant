//! Value types for the Samsung pairing wizard (`POST /api/pair/samsung` and
//! its poll endpoint, `GET /api/pair/samsung/{id}`).
//!
//! This file is intentionally types-only for now (Task 4 of the
//! config-crud-wizard plan): [`PairId`], [`PairStatus`], and [`Token`] are
//! defined here so [`crate::state::WebStateInner`]'s pairing fields (Task
//! 4b) can name them before the route HANDLERS land (Task 5). Until then
//! `PairId::new`/`Token::new`/`Token::expose_secret` are only reachable
//! from this module's own tests — expect transient `dead_code` warnings on
//! a lib-only, non-test build; they resolve once Task 5 wires the handlers
//! in. `dead_code` is allowed crate-locally in this module for the same
//! reason — clippy's `-D warnings` gate would otherwise hard-fail a
//! deliberately-not-yet-wired stub.
#![allow(dead_code)]

use std::fmt;
use std::fmt::Write as _;

use serde::Serialize;

/// Opaque identifier for one in-flight (or recently finished) pairing
/// attempt, handed back to the client in the `POST` response and used to
/// poll status.
///
/// Backed by 128 bits of randomness rendered as lowercase hex (32 chars) —
/// unguessable enough that a leaked `pair_id` isn't itself a credential.
/// The actual secret is the granted [`Token`], which is never echoed in a
/// `PairId` or anywhere else outside [`Token::expose_secret`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct PairId(String);

impl PairId {
    /// Generate a new random `PairId`.
    ///
    /// Uses `rand::random::<[u8; 16]>()` (the `rand` crate is already a
    /// normal dependency of this crate) hex-encoded by hand via `format!`
    /// — no new dependency (e.g. `uuid`/`hex`) is needed for 128 bits of
    /// randomness rendered as hex.
    #[must_use]
    pub(crate) fn new() -> Self {
        let bytes: [u8; 16] = rand::random();
        let hex = bytes.iter().fold(String::with_capacity(32), |mut acc, b| {
            write!(acc, "{b:02x}").expect("writing to a String never fails");
            acc
        });
        Self(hex)
    }
}

impl fmt::Display for PairId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Status of an in-flight or finished pairing attempt, returned by the poll
/// endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PairStatus {
    /// One of `"pairing"`, `"paired"`, `"timeout"`, `"error"`.
    pub(crate) state: &'static str,
    /// Optional human-readable detail (e.g. an error message). NEVER the
    /// token — see [`Token`]'s redaction.
    pub(crate) detail: Option<String>,
}

/// A pairing-granted access token.
///
/// The inner value is PRIVATE and `Debug`/`Display` are redacting
/// (`"***"`) so the token can never leak via `{:?}`/`{}` formatting (e.g.
/// an accidental `tracing::info!("{token:?}")` on a log-capture path). The
/// only sanctioned way to read the raw value is [`Token::expose_secret`] —
/// a single, grep-able bypass, never `.0`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Token(String);

impl Token {
    /// Wrap a raw token string.
    #[must_use]
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Explicitly extract the raw secret.
    #[must_use]
    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Token").field(&"***").finish()
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_id_new_produces_distinct_32_char_lowercase_hex() {
        let a = PairId::new();
        let b = PairId::new();
        assert_ne!(
            a, b,
            "two freshly generated PairIds should not collide in practice"
        );
        for id in [&a, &b] {
            let s = id.to_string();
            assert_eq!(
                s.len(),
                32,
                "PairId should render as 32 hex chars (128 bits): {s}"
            );
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "PairId should be lowercase hex: {s}"
            );
        }
    }

    #[test]
    fn token_debug_and_display_redact() {
        let token = Token::new("super-secret-value");
        let debug = format!("{token:?}");
        let display = format!("{token}");
        assert!(
            !debug.contains("super-secret-value"),
            "Debug leaked the token: {debug}"
        );
        assert!(
            !display.contains("super-secret-value"),
            "Display leaked the token: {display}"
        );
        assert!(debug.contains("***"), "Debug should redact to ***: {debug}");
        assert_eq!(display, "***");
    }

    #[test]
    fn token_expose_secret_returns_the_raw_value() {
        let token = Token::new("super-secret-value");
        assert_eq!(token.expose_secret(), "super-secret-value");
    }

    #[test]
    fn pair_status_serializes_each_expected_state() {
        for state in ["pairing", "paired", "timeout", "error"] {
            let status = PairStatus {
                state,
                detail: None,
            };
            let json = serde_json::to_string(&status).unwrap();
            assert!(
                json.contains(state),
                "serialized PairStatus should contain state {state}: {json}"
            );
        }
    }
}
