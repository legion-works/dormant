//! Shared IPC protocol types for dormant daemon ↔ CLI communication.
//!
//! Every request and response is a line-delimited JSON frame over a Unix
//! domain socket.  The types here are shared between `dormantd` (the server)
//! and `dormantctl` (the client) so the wire format has a single source of
//! truth.

use serde::{Deserialize, Serialize};

use crate::rules::StateSnapshot;

// ── IpcRequest ────────────────────────────────────────────────────────────────

/// A request from `dormantctl` to `dormantd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Fetch a full [`StateSnapshot`].
    Status,
    /// Pause blanking for an optional rule, optionally for a duration.
    Pause {
        /// Rule to pause (`None` → all rules).
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// Duration in seconds (`None` → indefinite).
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_s: Option<u64>,
    },
    /// Resume blanking for an optional rule.
    Resume {
        /// Rule to resume (`None` → all rules).
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
    },
    /// Force-blank a display.
    Blank {
        /// Display id.
        display: String,
    },
    /// Force-wake a display.
    Wake {
        /// Display id.
        display: String,
    },
    /// Subscribe to the live `DaemonEvent` stream.
    Events,
    /// Trigger a config reload.
    Reload,
}

// ── IpcResponse ───────────────────────────────────────────────────────────────

/// A response from `dormantd` to `dormantctl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    /// Whether the request succeeded.
    pub ok: bool,
    /// Human-readable error detail (present only when `ok` is `false`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A [`StateSnapshot`] (present only for `Status` responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<StateSnapshot>,
}

impl IpcResponse {
    /// Build a success response with an optional snapshot.
    #[must_use]
    pub fn ok(snapshot: Option<StateSnapshot>) -> Self {
        Self {
            ok: true,
            error: None,
            snapshot,
        }
    }

    /// Build an error response.
    #[must_use]
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            snapshot: None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── IpcRequest serde round-trips ───────────────────────────────────────

    #[test]
    fn request_status_serde() {
        let req = IpcRequest::Status;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"status"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, IpcRequest::Status));
    }

    #[test]
    fn request_pause_no_rule_no_duration_serde() {
        let req = IpcRequest::Pause {
            rule: None,
            duration_s: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"pause"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Pause { rule, duration_s } => {
                assert!(rule.is_none());
                assert!(duration_s.is_none());
            }
            _ => panic!("expected Pause"),
        }
    }

    #[test]
    fn request_pause_with_rule_and_duration_serde() {
        let req = IpcRequest::Pause {
            rule: Some("office".into()),
            duration_s: Some(7200),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"pause","rule":"office","duration_s":7200}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Pause { rule, duration_s } => {
                assert_eq!(rule.as_deref(), Some("office"));
                assert_eq!(duration_s, Some(7200));
            }
            _ => panic!("expected Pause"),
        }
    }

    #[test]
    fn request_resume_no_rule_serde() {
        let req = IpcRequest::Resume { rule: None };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"resume"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Resume { rule } => assert!(rule.is_none()),
            _ => panic!("expected Resume"),
        }
    }

    #[test]
    fn request_resume_with_rule_serde() {
        let req = IpcRequest::Resume {
            rule: Some("office".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"resume","rule":"office"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Resume { rule } => assert_eq!(rule.as_deref(), Some("office")),
            _ => panic!("expected Resume"),
        }
    }

    #[test]
    fn request_blank_serde() {
        let req = IpcRequest::Blank {
            display: "main".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"blank","display":"main"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Blank { display } => assert_eq!(display, "main"),
            _ => panic!("expected Blank"),
        }
    }

    #[test]
    fn request_wake_serde() {
        let req = IpcRequest::Wake {
            display: "tv".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"wake","display":"tv"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Wake { display } => assert_eq!(display, "tv"),
            _ => panic!("expected Wake"),
        }
    }

    #[test]
    fn request_events_serde() {
        let req = IpcRequest::Events;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"events"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, IpcRequest::Events));
    }

    #[test]
    fn request_reload_serde() {
        let req = IpcRequest::Reload;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"req":"reload"}"#);
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, IpcRequest::Reload));
    }

    // ── IpcResponse serde round-trips ──────────────────────────────────────

    #[test]
    fn response_ok_no_snapshot_serde() {
        let resp = IpcResponse::ok(None);
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true}"#);
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert!(back.error.is_none());
        assert!(back.snapshot.is_none());
    }

    #[test]
    fn response_error_serde() {
        let resp = IpcResponse::error("unknown display 'x'");
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":false,"error":"unknown display 'x'"}"#);
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("unknown display 'x'"));
        assert!(back.snapshot.is_none());
    }

    #[test]
    fn response_ok_with_snapshot_serde() {
        let snap = StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays: vec![],
            pending_reload: None,
        };
        let resp = IpcResponse::ok(Some(snap));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""snapshot""#));
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert!(back.snapshot.is_some());
    }

    #[test]
    fn response_parse_error_wire() {
        let json = r#"{"ok":false,"error":"bad request: parse error"}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("bad request: parse error"));
    }
}
